use async_recursion::async_recursion;
use console::Term;
use tracing::{error, info};
use std::{collections::HashMap, sync::{Arc, RwLock}};
use itertools::Itertools;

use dco3::{
    auth::Connected, nodes::{
        models::NodeType, rooms::models::CreateRoomRequest, GroupMemberAcceptance, Node, NodePermissions, Nodes, RoomGroupsAddBatchRequestItem, RoomPoliciesRequest, RoomUsersAddBatchRequestItem, Rooms
    }, user::UserAccount, Dracoon, ListAllParams
};

use super::models::{RoomImport, RoomId};

use crate::cmd::{
    init_dracoon, models::{DcCmdError, PasswordAuth}, utils::strings::parse_path
};


pub async fn create_room_structure(
    term: Term,
    source: String,
    classification: Option<u8>,
    path: String,
    auth: Option<PasswordAuth>
) -> Result<(), DcCmdError> {
    let room_struct = RoomImport::from_path(path)?;
    let dracoon = init_dracoon(&source, auth, false).await?;
    let (parent_path, node_name, _) = parse_path(&source, dracoon.get_base_url().as_ref())?;

    let path = parent_path.clone() + &node_name;

    let parent_node = dracoon
        .nodes
        .get_node_from_path(&path)
        .await?
        .ok_or(DcCmdError::InvalidPathOrNoPermission(source.clone()))?;

    if parent_node.node_type != NodeType::Room {
        return Err(DcCmdError::InvalidPath(source.clone()));
    }
    
    let current_user_acc = dracoon.get_user_info().await?;

    check_user_permissions_on_parent(&dracoon, &term, &parent_node, &path, current_user_acc.clone()).await?;

    info!("Creating room structure at path: {}/", &path);
    term.write_line(&std::format!("Creating room structure at path: {}/", &path))
        .expect("Error writing message to terminal.");

    // if the script user hasn't been given admin permissions in a room, we need to temporarily give it admin permissions and revoke them later or update the permissions of the script user 
    
    let rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>> = Arc::new(RwLock::new(HashMap::new()));
    create_rooms_and_subrooms_update_permissions_policies(&term, &dracoon, parent_node.id, room_struct, path, rooms_to_adjust_permissions.clone(), current_user_acc.id).await?;

    println!("rooms_to_adjust_permissions: {:?}", rooms_to_adjust_permissions);
    // revert permission changes for script user
    adjust_permissions(&term, &dracoon, rooms_to_adjust_permissions, current_user_acc.id).await?;
    Ok(())
}

async fn check_user_permissions_on_parent(dracoon: &Dracoon<Connected>, term: &Term, parent_node: &Node, path: &String, current_user_acc: UserAccount ) -> Result<(), DcCmdError> {
    info!("Checking permission of script user in room: {}", &path);
    term.write_line(&std::format!("Checking permission of script user in room: {}", &path))
        .expect("Error writing message to terminal.");

    let user_res = dracoon.nodes.get_room_users(parent_node.id, None).await?;
    let mut user_list = user_res.items;
    if user_res.range.total > 500 {
        let mut offset = 500;

        while offset < user_res.range.total {
            let params_with_offset = ListAllParams::builder()
                .with_offset(offset)
                .build();

            let user_res_offset = dracoon.nodes.get_room_users(parent_node.id, Some(params_with_offset)).await?;
            user_list.extend(user_res_offset.items);
            offset += 500;
        }
    }

    // todo fix i64 type in userinfo (dco3)
    if !user_list.iter().any(|user| u64::try_from(user.user_info.id).unwrap() == current_user_acc.id && user.permissions.clone().unwrap().manage) {
        error!("Script user does not have permission to create rooms in: {}", &path);
        term.write_line(&std::format!("Script user does not have permission to create rooms in: {}", &path))
            .expect("Error writing message to terminal.");
        return Err(DcCmdError::InsufficentPermissions(path.clone())); // todo change error type
    }

    Ok(())
}

#[async_recursion]
async fn create_rooms_and_subrooms_update_permissions_policies(
    term: &Term,
    dracoon: &Dracoon<Connected>,
    parent_id: u64,
    room_struct: Vec<RoomImport>,
    path: String,
    rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>,
    current_user_id: u64
) -> Result<(), DcCmdError> {
    for mut room in room_struct {
        let mut added_script_user_as_admin = false;
        // check if current_user_id is in admin_ids or is not in room.user_permissions then add it to admin_ids
        if room.admin_ids.is_none() || !room.admin_ids.clone().unwrap().contains(&current_user_id) || room.user_permissions.is_none() || !room.user_permissions.clone().unwrap().iter().any(|user| user.id == current_user_id){
            let mut admin_ids = room.admin_ids.unwrap_or(vec![]);
            admin_ids.push(current_user_id);
            admin_ids = admin_ids.into_iter().unique().collect();
            room.admin_ids = Some(admin_ids);
            added_script_user_as_admin = true;
        }

        let mut req = CreateRoomRequest::builder(&room.name)
            .with_parent_id(parent_id)
            .with_classification(room.classification.unwrap_or(2))
            .with_inherit_permissions(room.inherit_permissions.unwrap_or(false))
            .with_quota(room.quota.unwrap_or(0))
            .with_recycle_bin_retention_period(room.recycle_bin_retention_period.unwrap_or(0))
            .with_new_group_member_acceptance(room.new_group_member_acceptance.unwrap_or(GroupMemberAcceptance::AutoAllow));

        if let Some(admin_ids) = room.admin_ids {
            req = req.with_admin_ids(admin_ids);
        }   
        
        if let Some(admin_group_ids) = room.admin_group_ids {
            req = req.with_admin_group_ids(admin_group_ids);
        }

        let req = req.build();

        let created_room: Node = dracoon.nodes.create_room(req).await?;

        if added_script_user_as_admin {
            rooms_to_adjust_permissions.write().unwrap().insert(RoomId(created_room.id), None);
        }

        info!("Creatied room: {} at path: {}", room.name, path);
        term.write_line(&std::format!("Created room: {} at path: {}/", room.name, path))
            .expect("Error writing message to terminal.");

        // TODO put in separate function. Need to be able to clone Option<GroupMemberAcceptance>        
        if let Some(user_permissions) = room.user_permissions {
            let mut user_updates: Vec<RoomUsersAddBatchRequestItem> = vec![];
            
            for user_permission in user_permissions {
                if user_permission.id == current_user_id {
                    let mut permissions = user_permission.permissions;
                    // save original permissions to update them later
                    rooms_to_adjust_permissions.write().unwrap().insert(RoomId(created_room.id), Some(permissions.clone()));
                    // give temp admin right to script user
                    permissions.manage = true;
                    let user_update = RoomUsersAddBatchRequestItem::new(user_permission.id, permissions);
                    user_updates.push(user_update);
                } else {
                    let user_update  = RoomUsersAddBatchRequestItem::new(user_permission.id, user_permission.permissions);
                    user_updates.push(user_update);
                    }
                }
            dracoon.nodes.update_room_users(created_room.id, user_updates.into()).await?;
        }

        if let Some(group_permissions) = room.group_permissions {
            let mut group_updates: Vec<RoomGroupsAddBatchRequestItem> = vec![];
            for group_permission in group_permissions {
                let group_update = RoomGroupsAddBatchRequestItem::new(group_permission.id, group_permission.permissions, group_permission.new_group_member_acceptance);
                group_updates.push(group_update);
            }
            dracoon.nodes.update_room_groups(created_room.id, group_updates.into()).await?;
        }

        if let Some(policies) = room.policies {
            println!("Policies: {:?}", policies);
            let mut new_policies = RoomPoliciesRequest::builder()
                .with_default_expiration_period(policies.default_expiration_period.unwrap_or(0));

            if let Some(is_virus_protection_enabled) = policies.is_virus_protection_enabled {         
                new_policies = new_policies.with_virus_protection_enabled(is_virus_protection_enabled);
            }
            let new_policies = new_policies.build();
            dracoon.nodes.update_room_policies(created_room.id, new_policies).await?;
        }

        info!("Subrooms: {:?}", room.sub_rooms);
        term.write_line(&std::format!("Subrooms: {:?}", room.sub_rooms))
            .expect("Error writing message to terminal.");
      
        if let Some(sub_rooms) = room.sub_rooms {
            println!("Subrooms: {:?}", sub_rooms); 
            let new_parent_path = created_room.parent_path.unwrap() + &created_room.name;
            create_rooms_and_subrooms_update_permissions_policies(term, dracoon, created_room.id, sub_rooms, new_parent_path, rooms_to_adjust_permissions.clone(), current_user_id).await?
        }
    }
    Ok(())
}

async fn adjust_permissions(term: &Term, dracoon: &Dracoon<Connected>, rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>, current_user_id: u64) -> Result<(), DcCmdError>{
    // TO DO: consider using an async-aware `Mutex` type or ensuring the `MutexGuard` is dropped before calling await
    for (room_id, permissions) in rooms_to_adjust_permissions.read().unwrap().iter() {
        if let Some(permissions) = permissions {
            let user_update = RoomUsersAddBatchRequestItem::new(current_user_id, permissions.clone());
            dracoon.nodes.update_room_users(room_id.0, vec![user_update].into()).await?;
        } else {
            dracoon.nodes.delete_room_users(room_id.0, vec![current_user_id].into()).await?;
        }
    }

    Ok(())
}