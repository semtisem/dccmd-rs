use async_recursion::async_recursion;
use console::Term;
use indicatif::{ProgressBar, ProgressStyle};
use itertools::Itertools;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{debug, error, info};

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

use super::models::{
    GroupRoomPermission, Room, RoomId, RoomImport, RoomPolicies, UpdateTask, UpdateTaskType,
    UpdateTasksChannel, UserRoomPermission,
};

use crate::cmd::{
    init_dracoon,
    models::{DcCmdError, PasswordAuth},
    utils::strings::parse_path,
};

pub async fn import_and_create_room_structure(
    term: Term,
    source: String,
    json_path: String,
    auth: Option<PasswordAuth>,
) -> Result<(), DcCmdError> {
    let dracoon = init_dracoon(&source, auth, false).await?;

    let room_import = RoomImport::from_path(json_path, &dracoon, term.clone()).await?;

    let current_user_acc = dracoon.get_user_info().await?;

    let (parent_id, path) =
        validate_node_path_and_get_parent_id(&source, &dracoon, &term, current_user_acc.clone())
            .await?;

    let mut update_room_tasks_channel = UpdateTasksChannel::new();

    let rooms_to_adjust_permissions = process_rooms_wrapper(
        term.clone(),
        dracoon.clone(),
        room_import.clone(),
        room_import.total_room_count,
        parent_id,
        path,
        current_user_acc.clone(),
        update_room_tasks_channel.get_sender(),
    )
    .await;

    update_room_tasks_channel
        .collect_than_complete(term.clone(), &dracoon)
        .await;

    // revert permission changes for user
    adjust_temp_admin_permissions(
        term,
        dracoon.clone(),
        rooms_to_adjust_permissions.clone(),
        current_user_acc.id,
    )
    .await?;

    Ok(())
}
async fn process_rooms_wrapper(
    term: Term,
    dracoon: Dracoon<Connected>,
    room_import: RoomImport,
    total_room_count: u64,
    parent_id: Option<u64>,
    path: String,
    current_user_acc: UserAccount,
    sender: mpsc::Sender<UpdateTask>,
) -> Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>> {
    // if the user hasn't been given admin permissions in a room, we need to temporarily give it admin permissions and revoke them later or update the permissions of the user
    let rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    info!("Creating room structure at path: {}/", &path);
    term.write_line(&std::format!("Creating room structure at path: {}/", &path))
        .expect("Error writing message to terminal.");

    let progress_bar = ProgressBar::new(total_room_count);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );
    progress_bar.set_length(total_room_count);
    let message = format!("Creating room structure");
    progress_bar.set_message(message.clone());

    let res = process_rooms(
        term.clone(),
        dracoon.clone(),
        parent_id,
        room_import.rooms,
        path.clone(),
        rooms_to_adjust_permissions.clone(),
        current_user_acc.id,
        sender,
        progress_bar.clone(),
    )
    .await;

    progress_bar.finish_with_message(format!(
        "Created {total_room_count} rooms successfully. Adjusting permissions and policies now"
    ));

    rooms_to_adjust_permissions
}

async fn validate_node_path_and_get_parent_id(
    source: &String,
    dracoon: &Dracoon<Connected>,
    term: &Term,
    current_user_acc: UserAccount,
) -> Result<(Option<u64>, String), DcCmdError> {
    let (parent_path, node_name, _) = parse_path(&source, dracoon.get_base_url().as_ref())?;
    let mut parent_id = None;
    let path = format!("{}{}", &parent_path, &node_name);

    // check if root
    if !node_name.is_empty() {
        let parent_node = dracoon
            .nodes
            .get_node_from_path(&path)
            .await?
            .ok_or(DcCmdError::InvalidPathOrNoPermission(source.clone()))?;

        if parent_node.node_type != NodeType::Room {
            return Err(DcCmdError::InvalidPath(source.clone()));
        }

        parent_id = Some(parent_node.id);
        Ok((parent_id, path))
    } else {
        check_user_permissions_on_parent(
            &dracoon,
            &term,
            parent_id,
            &path,
            current_user_acc.clone(),
        )
        .await?;
        Ok((parent_id, path))
    }
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
async fn process_rooms(
    term: Term,
    dracoon: Dracoon<Connected>,
    parent_id: Option<u64>,
    room_struct: Vec<Room>,
    path: String,
    rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>,
    current_user_id: u64,
    room_update_tasks_sender: mpsc::Sender<UpdateTask>,
    progress_bar: ProgressBar,
) -> Result<(), DcCmdError> {
    let mut room_tasks: JoinSet<Result<Node, DcCmdError>> = JoinSet::new();

    for room in room_struct {
        // need clone bc of move
        let path = path.clone();
        let term = term.clone();
        let rooms_to_adjust_permissions = rooms_to_adjust_permissions.clone();
        let dracoon = dracoon.clone();
        let room_update_tasks_sender = room_update_tasks_sender.clone();
        let progress_bar = progress_bar.clone();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        room_tasks.spawn(async move {
            let created_room = process_room(
                term.clone(),
                dracoon.clone(),
                parent_id.clone(),
                room.clone(),
                path.clone(),
                rooms_to_adjust_permissions,
                current_user_id.clone(),
                room_update_tasks_sender.clone(),
                progress_bar.clone(),
            )
            .await?;
            progress_bar.inc(1);
            Ok(created_room)
        });
    }

    while let Some(res) = room_tasks.join_next().await {
        match res {
            Ok(task_res) => match task_res {
                Ok(node) => {
                    let message = format!("Created room: {} at path: {}", node.name, path);
                    info!("{}", message);
                    progress_bar.set_message(message);
                }
                Err(e) => {
                    error!("Error creating room: {}", e);
                    term.write_line(&std::format!("Error creating room: {}", e))
                        .expect("Error writing message to terminal.");
                }
            },
            Err(e) => {
                error!("Error creating room: {}", e);
                term.write_line(&std::format!("Error creating room: {}", e))
                    .expect("Error writing message to terminal.");
            }
        }
    }

    Ok(())
}

async fn process_room(
    term: Term,
    dracoon: Dracoon<Connected>,
    parent_id: Option<u64>,
    mut room: Room,
    path: String,
    rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>,
    current_user_id: u64,
    room_update_tasks_sender: mpsc::Sender<UpdateTask>,
    progress_bar: ProgressBar,
) -> Result<Node, DcCmdError> {
    let created_room = create_room(
        term.clone(),
        dracoon.clone(),
        parent_id,
        &mut room,
        &path,
        rooms_to_adjust_permissions.clone(),
        current_user_id,
    )
    .await?;

    let created_room_id = created_room.id.clone();

    update_room_groups(
        term.clone(),
        created_room_id,
        room.group_permissions.clone(),
        room_update_tasks_sender.clone(),
    )
    .await?;

    update_room_users(
        term.clone(),
        created_room_id.clone(),
        room.user_permissions.clone(),
        rooms_to_adjust_permissions.clone(),
        current_user_id,
        room_update_tasks_sender.clone(),
    )
    .await?;

    update_room_policies(
        term.clone(),
        created_room_id.clone(),
        room.policies.clone(),
        room_update_tasks_sender.clone(),
    )
    .await?;

    if let Some(sub_rooms) = room.sub_rooms.clone() {
        let new_parent_path = created_room.parent_path.clone().unwrap() + &created_room.name;
        process_rooms(
            term.clone(),
            dracoon.clone(),
            Some(created_room.id),
            sub_rooms,
            new_parent_path,
            rooms_to_adjust_permissions.clone(),
            current_user_id,
            room_update_tasks_sender,
            progress_bar.clone(),
        )
        .await?
    }

    Ok(created_room)
}

async fn adjust_temp_admin_permissions(
    term: Term,
    dracoon: Dracoon<Connected>,
    rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>,
    current_user_id: u64,
) -> Result<(), DcCmdError> {
    let total_size = rooms_to_adjust_permissions.read().await.len() as u64;
    let progress_bar = ProgressBar::new(total_size);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );
    progress_bar.set_length(total_size as u64);
    let message = format!("Adjusting permissions of script user in rooms");
    progress_bar.set_message(message.clone());
    static MAX_CONCURRENT: usize = 2;

    let mut join_set = JoinSet::new();
    // todo check how different futures can be held in a single vec
    let mut batch_requests_update = Vec::new();
    let mut batch_requests_delete = Vec::new();

    for (room_id, permissions) in rooms_to_adjust_permissions.read().await.iter() {
        let room_id = room_id.clone();
        let permissions = permissions.clone();
        let dracoon = dracoon.clone();
        let progress_bar_clone = progress_bar.clone();
        if let Some(permissions) = permissions {
            let user_update =
                RoomUsersAddBatchRequestItem::new(current_user_id, permissions.clone());
            let task = async move {
                let res = dracoon
                    .nodes
                    .update_room_users(room_id.0.clone(), vec![user_update].into())
                    .await;
                // to do handle error
                progress_bar_clone.inc(1);
                res
            };
            batch_requests_update.push(task);
        } else {
            let task = async move {
                let res = dracoon
                    .nodes
                    .delete_room_users(room_id.0, vec![current_user_id].into())
                    .await;
                // to do handle error
                progress_bar_clone.inc(1);
                res
            };
            batch_requests_delete.push(task)
        }
    }

    for request in batch_requests_update {
        while join_set.len() >= MAX_CONCURRENT {
            while let Some(res) = join_set.join_next().await {
                match res {
                    Ok(_) => {
                        info!("Permissions adjusted successfully");
                    }
                    Err(e) => {
                        error!("Error adjusting permissions: {}", e);
                        term.write_line(&std::format!("Error adjusting permissions: {}", e))
                            .expect("Error writing message to terminal.");
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        join_set.spawn(request);
    }

    for request in batch_requests_delete {
        while join_set.len() >= MAX_CONCURRENT {
            while let Some(res) = join_set.join_next().await {
                match res {
                    Ok(_) => {
                        info!("Permissions adjusted successfully");
                    }
                    Err(e) => {
                        error!("Error adjusting permissions: {}", e);
                        term.write_line(&std::format!("Error adjusting permissions: {}", e))
                            .expect("Error writing message to terminal.");
                    }
                }
            }
        }
        // wait for 200ms before adding new task to join_set to avoid 500 error
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        join_set.spawn(request);
    }

    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(_) => {
                info!("Permissions adjusted successfully");
            }
            Err(e) => {
                error!("Error adjusting permissions: {}", e);
                term.write_line(&std::format!("Error adjusting permissions: {}", e))
                    .expect("Error writing message to terminal.");
            }
        }
    }

    progress_bar.finish_with_message(format!("Successfully adjusted {total_size} permission(s)"));

    Ok(())
}

async fn create_room(
    term: Term,
    dracoon: Dracoon<Connected>,
    parent_id: Option<u64>,
    room: &mut Room,
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
            .await
            .insert(RoomId(created_room.id), permissions_to_adjust);
    }

    info!("Created room: {} at path: {}", room.name, path);
    Ok(created_room)
}

async fn update_room_users(
    term: Term,
    room_id: u64,
    user_permissions: Option<Vec<UserRoomPermission>>,
    rooms_to_adjust_permissions: Arc<RwLock<HashMap<RoomId, Option<NodePermissions>>>>,
    current_user_id: u64,
    room_update_tasks_sender: mpsc::Sender<UpdateTask>,
) -> Result<(), DcCmdError> {
    if let Some(user_permissions) = user_permissions {
        let mut user_updates: Vec<RoomUsersAddBatchRequestItem> = vec![];
        for user_permission in user_permissions {
            if user_permission.id == current_user_id {
                let mut permissions = user_permission.permissions;
                // save original permissions to update them later
                rooms_to_adjust_permissions
                    .write()
                    .await
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

        let res = room_update_tasks_sender
            .send(UpdateTask {
                task_type: UpdateTaskType::RoomUser(RoomId(room_id), user_updates),
            })
            .await;

        match res {
            Ok(_) => {
                debug!("Send task to update user permissions for room: {}", room_id);
            }
            Err(e) => {
                error!(
                    "Reciever closed. Error sending task to update user permissions for room: {}",
                    e
                );
                term.write_line(&std::format!(
                    "Reciever closed. Error sending task to update user permissions for room: {}",
                    e
                ))
                .expect("Error writing message to terminal.");
            }
        }
    } else {
        debug!("No user permissions to update for room: {}", room_id);
    }

    Ok(())
}

async fn update_room_groups(
    term: Term,
    room_id: u64,
    group_permissions: Option<Vec<GroupRoomPermission>>,
    room_update_tasks_sender: mpsc::Sender<UpdateTask>,
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

        let res = room_update_tasks_sender
            .send(UpdateTask {
                task_type: UpdateTaskType::RoomGroup(RoomId(room_id), group_updates),
            })
            .await;

        match res {
            Ok(_) => {
                debug!(
                    "Send task to update group permissions for room: {}",
                    room_id
                );
            }
            Err(e) => {
                error!(
                    "Reciever closed. Error sending task to update group permissions for room: {}",
                    e
                );
                term.write_line(&std::format!(
                    "Reciever closed. Error sending task to update group permissions for room: {}",
                    e
                ))
                .expect("Error writing message to terminal.");
            }
        }
    } else {
        debug!("No task to update group permissions for room: {}", room_id);
    }

    Ok(())
}

async fn update_room_policies(
    term: Term,
    room_id: u64,
    policies: Option<RoomPolicies>,
    room_update_tasks_sender: mpsc::Sender<UpdateTask>,
) -> Result<(), DcCmdError> {
    if let Some(policies) = policies {
        let mut new_policies = RoomPoliciesRequest::builder()
            .with_default_expiration_period(policies.default_expiration_period.unwrap_or(0));

        if let Some(is_virus_protection_enabled) = policies.is_virus_protection_enabled {
            new_policies = new_policies.with_virus_protection_enabled(is_virus_protection_enabled);
        }
        let new_policies = new_policies.build();

        let res = room_update_tasks_sender
            .send(UpdateTask {
                task_type: UpdateTaskType::RoomPolicies(RoomId(room_id), new_policies),
            })
            .await;

        match res {
            Ok(_) => {
                debug!("Send task to update room policies for room: {}", room_id);
            }
            Err(e) => {
                error!(
                    "Reciever closed. Error sending task to update room policies for room: {}",
                    e
                );
                term.write_line(&std::format!(
                    "Reciever closed. Error sending task to update room policies for room: {}",
                    e
                ))
                .expect("Error writing message to terminal.");
            }
        }
    } else {
        debug!("No room policies to update for room: {}", room_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_update_room_policies() {
        let term = Term::stdout();
        let room_id = 1;
        let policies = RoomPolicies {
            default_expiration_period: Some(0),
            is_virus_protection_enabled: Some(true),
        };

        let mut room_update_task_channel = UpdateTasksChannel::new();
        let room_update_tasks_sender = room_update_task_channel.get_sender();
        let res = update_room_policies(
            term.clone(),
            room_id,
            Some(policies),
            room_update_tasks_sender,
        )
        .await
        .unwrap();
        assert_eq!(res, ());

        room_update_task_channel.collect_tasks(term).await;

        assert_eq!(room_update_task_channel.get_policy_task_count(), 1);
    }
}
