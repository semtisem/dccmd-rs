use async_recursion::async_recursion;
use console::Term;
use tracing::info;


use dco3::{
    auth::Connected,
    nodes::{
        models::NodeType, rooms::models::CreateRoomRequest, Node, Nodes, RoomGroupsAddBatchRequestItem, RoomUsersAddBatchRequestItem, Rooms
    },
    Dracoon,
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
        .ok_or(DcCmdError::InvalidPath(source.clone()))?;

    if parent_node.node_type != NodeType::Room {
        return Err(DcCmdError::InvalidPath(source.clone()));
    }

    info!("Creating room structure in room: {}", parent_node.id);
    term.write_line(&std::format!("Creating room structure at path: {}/", &path))
        .expect("Error writing message to terminal.");

    Ok(create_rooms_and_subrooms(&term, &dracoon, parent_node.id, room_struct, path).await?)

    

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
        let mut req = CreateRoomRequest::builder(&room.name)
            .with_parent_id(parent_id)
            .with_classification(room.classification.unwrap_or(2));

        if let Some(inherit_permissions) = room.inherit_permissions {
            req = req.with_inherit_permissions(inherit_permissions);
        };
        
        if let Some(quota) = room.quota {
            req = req.with_quota(quota);
        };

        if let Some(recycle_bin_retention_period) = room.recycle_bin_retention_period {
            req = req.with_recycle_bin_retention_period(recycle_bin_retention_period);
        };

        if let Some(admin_ids) = room.admin_ids {
            // todo if admin id is set and different from the current user, add the admin id
            // save the the room id where the script user needs to remove itself as admin
            req = req.with_admin_ids(admin_ids);
        };

        if let Some(admin_group_ids) = room.admin_group_ids {
            req = req.with_admin_group_ids(admin_group_ids);
        };

        if let Some(new_group_member_acceptance) = room.new_group_member_acceptance {
            req = req.with_new_group_member_acceptance(new_group_member_acceptance);
        };

        let req = req.build();



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