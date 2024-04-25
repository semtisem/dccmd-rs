use async_recursion::async_recursion;
use console::Term;
use itertools::Itertools;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tracing::{error, info};

use dco3::{
    auth::Connected,
    nodes::{
        models::NodeType, rooms::models::CreateRoomRequest, GroupMemberAcceptance, Node,
        NodePermissions, Nodes, RoomGroupsAddBatchRequestItem, RoomPoliciesRequest,
        RoomUsersAddBatchRequestItem, Rooms,
    },
    user::UserAccount,
    Dracoon, ListAllParams,
};

use super::models::{GroupRoomPermission, RoomId, RoomImport, RoomPolicies, UserRoomPermission};

use crate::cmd::{
    init_dracoon,
    models::{DcCmdError, PasswordAuth},
    utils::strings::parse_path,
};

pub async fn create_room_structure(
    term: Term,
    source: String,
    classification: Option<u8>,
    path: String,
    auth: Option<PasswordAuth>,
) -> Result<(), DcCmdError> {
    let room_struct = RoomImport::from_path(path)?;
    let dracoon = init_dracoon(&source, auth, false).await?;
    let current_user_acc = dracoon.get_user_info().await?;
    let (parent_path, node_name, _) = parse_path(&source, dracoon.get_base_url().as_ref())?;
    let mut parent_id = None;
    let path = format!("{}{}", &parent_path, &node_name);

    // check if root
    if node_name.len() > 0 {
        let parent_node = dracoon
            .nodes
            .get_node_from_path(&path)
            .await?
            .ok_or(DcCmdError::InvalidPathOrNoPermission(source.clone()))?;

        if parent_node.node_type != NodeType::Room {
            return Err(DcCmdError::InvalidPath(source.clone()));
        }

        parent_id = Some(parent_node.id);
    } else {
        check_user_permissions_on_parent(
            &dracoon,
            &term,
            parent_id,
            &path,
            current_user_acc.clone(),
        )
        .await?;
    }

    info!("Creating room structure at path: {}/", &path);
    term.write_line(&std::format!("Creating room structure at path: {}/", &path))
        .expect("Error writing message to terminal.");

    // if the user hasn't been given admin permissions in a room, we need to temporarily give it admin permissions and revoke them later or update the permissions of the user
    // To check if rw lock is okay
    let rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    create_rooms_and_subrooms_update_permissions_policies(
        &term,
        &dracoon,
        parent_id,
        room_struct,
        path,
        rooms_to_adjust_permissions.clone(),
        current_user_acc.id,
    )
    .await?;

    // revert permission changes for user
    adjust_permissions(
        &term,
        &dracoon,
        rooms_to_adjust_permissions,
        current_user_acc.id,
    )
    .await?;
    Ok(())
}

async fn check_user_permissions_on_parent(
    dracoon: &Dracoon<Connected>,
    term: &Term,
    parent_id: Option<u64>,
    path: &String,
    current_user_acc: UserAccount,
) -> Result<(), DcCmdError> {
    info!("Checking permission of user in room: {}", &path);
    term.write_line(&std::format!(
        "Checking permission of user in room: {}",
        &path,
    ))
    .expect("Error writing message to terminal.");

    if parent_id.is_none() {
        let has_role_room_manager = current_user_acc
            .user_roles
            .items
            .iter()
            .any(|role| role.name == "ROOM_MANAGER");
        if !has_role_room_manager {
            error!(
                "user does not have permission to create rooms in: {} - User needs room manager role",
                &path
            );
            term.write_line(&std::format!(
                "user does not have permission to create rooms in: {} - User needs room manager role",
                &path
            ))
            .expect("Error writing message to terminal.");
            return Err(DcCmdError::InsufficentPermissions(path.clone()));
        } else {
            return Ok(());
        }
    }

    let parent_id = match parent_id {
        Some(id) => id,
        None => {
            return Err(DcCmdError::InvalidPath(path.clone()));
        }
    };

    let user_res = dracoon.nodes.get_room_users(parent_id, None).await?;
    let mut user_list = user_res.items;
    if user_res.range.total > 500 {
        let mut offset = 500;

        while offset < user_res.range.total {
            let params_with_offset = ListAllParams::builder().with_offset(offset).build();

            let user_res_offset = dracoon
                .nodes
                .get_room_users(parent_id, Some(params_with_offset))
                .await?;
            user_list.extend(user_res_offset.items);
            offset += 500;
        }
    }

    if !user_list.iter().any(|user| {
        u64::try_from(user.user_info.id).unwrap() == current_user_acc.id
            && user.permissions.clone().unwrap().manage
    }) {
        error!(
            "user does not have permission to create rooms in: {}",
            &path
        );
        term.write_line(&std::format!(
            "user does not have permission to create rooms in: {}",
            &path
        ))
        .expect("Error writing message to terminal.");
        return Err(DcCmdError::InsufficentPermissions(path.clone())); // todo change error type
    }

    Ok(())
}

#[async_recursion]
async fn create_rooms_and_subrooms_update_permissions_policies(
    term: &Term,
    dracoon: &Dracoon<Connected>,
    parent_id: Option<u64>,
    room_struct: Vec<RoomImport>,
    path: String,
    rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>,
    current_user_id: u64,
) -> Result<(), DcCmdError> {
    for mut room in room_struct {
        let created_room: Node = create_room(
            term,
            dracoon,
            parent_id,
            &mut room,
            &path,
            rooms_to_adjust_permissions.clone(),
            current_user_id,
        )
        .await?;

        update_room_users(
            term,
            dracoon,
            created_room.id,
            room.user_permissions,
            rooms_to_adjust_permissions.clone(),
            current_user_id,
        )
        .await?;

        update_room_groups(term, dracoon, created_room.id, room.group_permissions).await?;

        update_room_policies(term, dracoon, created_room.id, room.policies).await?;

        if let Some(sub_rooms) = room.sub_rooms {
            let new_parent_path = created_room.parent_path.unwrap() + &created_room.name;
            create_rooms_and_subrooms_update_permissions_policies(
                term,
                dracoon,
                Some(created_room.id),
                sub_rooms,
                new_parent_path,
                rooms_to_adjust_permissions.clone(),
                current_user_id,
            )
            .await?
        }
    }
    Ok(())
}

async fn adjust_permissions(
    term: &Term,
    dracoon: &Dracoon<Connected>,
    rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>,
    current_user_id: u64,
) -> Result<(), DcCmdError> {
    // TO DO: consider using an async-aware `Mutex` type or ensuring the `MutexGuard` is dropped before calling await
    for (room_id, permissions) in rooms_to_adjust_permissions.read().unwrap().iter() {
        info!("Adjusting permissions for user in room: {}", room_id.0);
        term.write_line(&std::format!(
            "Adjusting permissions for user in room: {}, {}",
            room_id.0,
            permissions.is_some()
        ))
        .expect("Error writing message to terminal.");
        if let Some(permissions) = permissions {
            info!("Updating user permissions in room: {}", room_id.0);
            let user_update =
                RoomUsersAddBatchRequestItem::new(current_user_id, permissions.clone());
            dracoon
                .nodes
                .update_room_users(room_id.0, vec![user_update].into())
                .await?;
        } else {
            info!("Deleting user from room: {}", room_id.0);
            term.write_line(&std::format!("Deleting user from room: {}", room_id.0))
                .expect("Error writing message to terminal.");
            dracoon
                .nodes
                .delete_room_users(room_id.0, vec![current_user_id].into())
                .await?;
        }
    }

    Ok(())
}

async fn create_room(
    term: &Term,
    dracoon: &Dracoon<Connected>,
    parent_id: Option<u64>,
    room: &mut RoomImport,
    path: &String,
    rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>,
    current_user_id: u64,
) -> Result<Node, DcCmdError> {
    let mut adjust_permissions = false;
    let mut permissions_to_adjust: Option<NodePermissions> = None;

    if room.admin_ids.is_none() || !room.admin_ids.clone().unwrap().contains(&current_user_id) {
        let mut admin_ids = room.admin_ids.clone().unwrap_or(vec![]);
        admin_ids.push(current_user_id);
        admin_ids = admin_ids.into_iter().unique().collect();
        room.admin_ids = Some(admin_ids);
        adjust_permissions = true;

        // check if user is in user_permissions and save permissions to adjust later
        if let Some(user_permissions) = &room.user_permissions {
            if let Some(permission) = user_permissions
                .iter()
                .find(|user| user.id == current_user_id)
            {
                permissions_to_adjust = Some(permission.permissions.clone());
            }
        }
    }

    let mut req = CreateRoomRequest::builder(&room.name)
        .with_classification(room.classification.unwrap_or(2))
        .with_inherit_permissions(room.inherit_permissions.unwrap_or(false))
        .with_quota(room.quota.unwrap_or(0))
        .with_recycle_bin_retention_period(room.recycle_bin_retention_period.unwrap_or(0));

    // check if root or not
    if let Some(parent_id) = parent_id {
        req = req.with_parent_id(parent_id);
    } else {
        // needs to be disabled on root
        req = req.with_inherit_permissions(false);
    }

    if let Some(new_group_member_acceptance) = &room.new_group_member_acceptance {
        let string = format!("{:?}", new_group_member_acceptance);
        let new_group_member_acceptance = match string.as_str() {
            "AutoAllow" => GroupMemberAcceptance::AutoAllow,
            "Pending" => GroupMemberAcceptance::Pending,
            _ => GroupMemberAcceptance::AutoAllow,
        };
        req = req.with_new_group_member_acceptance(new_group_member_acceptance);
    } else {
        req = req.with_new_group_member_acceptance(GroupMemberAcceptance::AutoAllow);
    }

    if let Some(admin_ids) = &room.admin_ids {
        req = req.with_admin_ids(admin_ids.clone());
    }

    if let Some(admin_group_ids) = &room.admin_group_ids {
        req = req.with_admin_group_ids(admin_group_ids.clone());
    }

    let req = req.build();

    let created_room: Node = dracoon.nodes.create_room(req).await?;

    if adjust_permissions {
        rooms_to_adjust_permissions
            .write()
            .unwrap()
            .insert(RoomId(created_room.id), permissions_to_adjust);
    }

    info!("Creatied room: {} at path: {}", room.name, path);
    term.write_line(&std::format!(
        "Created room: {} at path: {}/",
        room.name,
        path
    ))
    .expect("Error writing message to terminal.");

    Ok(created_room)
}

async fn update_room_users(
    term: &Term,
    dracoon: &Dracoon<Connected>,
    room_id: u64,
    user_permissions: Option<Vec<UserRoomPermission>>,
    rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>,
    current_user_id: u64,
) -> Result<(), DcCmdError> {
    if let Some(user_permissions) = user_permissions {
        let mut user_updates: Vec<RoomUsersAddBatchRequestItem> = vec![];
        for user_permission in user_permissions {
            if user_permission.id == current_user_id {
                let mut permissions = user_permission.permissions;
                // save original permissions to update them later
                rooms_to_adjust_permissions
                    .write()
                    .unwrap()
                    .insert(RoomId(room_id), Some(permissions.clone()));
                // give temp admin right to user
                permissions.manage = true;
                let user_update =
                    RoomUsersAddBatchRequestItem::new(user_permission.id, permissions);
                user_updates.push(user_update);
            } else {
                let user_update = RoomUsersAddBatchRequestItem::new(
                    user_permission.id,
                    user_permission.permissions,
                );
                user_updates.push(user_update);
            }
        }
        dracoon
            .nodes
            .update_room_users(room_id, user_updates.into())
            .await?;
    }
    Ok(())
}

async fn update_room_groups(
    term: &Term,
    dracoon: &Dracoon<Connected>,
    room_id: u64,
    group_permissions: Option<Vec<GroupRoomPermission>>,
) -> Result<(), DcCmdError> {
    if let Some(group_permissions) = group_permissions {
        let mut group_updates: Vec<RoomGroupsAddBatchRequestItem> = vec![];
        for group_permission in group_permissions {
            let group_update = RoomGroupsAddBatchRequestItem::new(
                group_permission.id,
                group_permission.permissions,
                group_permission.new_group_member_acceptance,
            );
            group_updates.push(group_update);
        }
        dracoon
            .nodes
            .update_room_groups(room_id, group_updates.into())
            .await?;
    }
    Ok(())
}

async fn update_room_policies(
    term: &Term,
    dracoon: &Dracoon<Connected>,
    room_id: u64,
    policies: Option<RoomPolicies>,
) -> Result<(), DcCmdError> {
    if let Some(policies) = policies {
        let mut new_policies = RoomPoliciesRequest::builder()
            .with_default_expiration_period(policies.default_expiration_period.unwrap_or(0));

        if let Some(is_virus_protection_enabled) = policies.is_virus_protection_enabled {
            new_policies = new_policies.with_virus_protection_enabled(is_virus_protection_enabled);
        }
        let new_policies = new_policies.build();
        dracoon
            .nodes
            .update_room_policies(room_id, new_policies)
            .await?;
    }

    Ok(())
}
