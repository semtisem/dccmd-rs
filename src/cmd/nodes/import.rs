use async_recursion::async_recursion;
use console::Term;
use tracing::{error, info};


use dco3::{
    auth::Connected, nodes::{
        models::NodeType, rooms::models::CreateRoomRequest, GroupMemberAcceptance, Node, Nodes, RoomGroupsAddBatchRequestItem, RoomUsersAddBatchRequestItem, Rooms
    }, Dracoon, ListAllParams
};

use super::models::RoomImport;

use crate::cmd::{
    init_dracoon,
    models::{DcCmdError, PasswordAuth}, utils::strings::parse_path,
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
    
    check_user_permissions(&dracoon, &term, &parent_node, &path).await?;

    info!("Creating room structure at path: {}/", &path);
    term.write_line(&std::format!("Creating room structure at path: {}/", &path))
        .expect("Error writing message to terminal.");

    Ok(create_rooms_and_subrooms(&term, &dracoon, parent_node.id, room_struct, path).await?)

    

}

async fn check_user_permissions(dracoon: &Dracoon<Connected>, term: &Term, parent_node: &Node, path: &String) -> Result<(), DcCmdError> {
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

    let current_user = dracoon.get_user_info().await?;

    // todo fix i64 type in userinfo (dco3)
    if !user_list.iter().any(|user| u64::try_from(user.user_info.id).unwrap() == current_user.id && user.permissions.clone().unwrap().manage) {
        error!("Script user does not have permission to create rooms in: {}", &path);
        term.write_line(&std::format!("Script user does not have permission to create rooms in: {}", &path))
            .expect("Error writing message to terminal.");
        return Err(DcCmdError::InsufficentPermissions(path.clone())); // todo change error type
    }

    Ok(())
}

#[async_recursion]
async fn create_rooms_and_subrooms(
    term: &Term,
    dracoon: &Dracoon<Connected>,
    parent_id: u64,
    room_struct: Vec<RoomImport>,
    path: String
) -> Result<(), DcCmdError> {
    for room in room_struct {
        let req = CreateRoomRequest::builder(&room.name)
            .with_parent_id(parent_id)
            .with_classification(room.classification.unwrap_or(2))
            .with_inherit_permissions(room.inherit_permissions.unwrap_or(false))
            .with_quota(room.quota.unwrap_or(0))
            .with_recycle_bin_retention_period(room.recycle_bin_retention_period.unwrap_or(0))
            .with_admin_ids(room.admin_ids.unwrap_or(vec![]))
            .with_admin_group_ids(room.admin_group_ids.unwrap_or(vec![]))
            .with_new_group_member_acceptance(room.new_group_member_acceptance.unwrap_or(GroupMemberAcceptance::AutoAllow))
            .build();

        let created_room: Node = dracoon.nodes.create_room(req).await?;

        info!("Creatied room: {} at path: {}", room.name, path);
        term.write_line(&std::format!("Created room: {} at path: {}/", room.name, path))
            .expect("Error writing message to terminal.");

        if let Some(user_permissions) = room.user_permissions {
            let mut user_updates: Vec<RoomUsersAddBatchRequestItem> = vec![];
            for user_permission in user_permissions {
                let user_update = RoomUsersAddBatchRequestItem::new(user_permission.id, user_permission.permissions);
                user_updates.push(user_update);
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

        info!("Subrooms: {:?}", room.sub_rooms);
        term.write_line(&std::format!("Subrooms: {:?}", room.sub_rooms))
            .expect("Error writing message to terminal.");
      
    
        if let Some(sub_rooms) = room.sub_rooms {
            println!("Subrooms: {:?}", sub_rooms); 
            let new_parent_path = created_room.parent_path.unwrap() + &created_room.name;
            create_rooms_and_subrooms(term, dracoon, created_room.id, sub_rooms, new_parent_path ).await?
        }
    }
    Ok(())
}