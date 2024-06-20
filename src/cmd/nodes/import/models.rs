use console::Term;
use csv::Reader;
use dco3::nodes::{
    GroupMemberAcceptance, NodePermissions, RoomGroupsAddBatchRequestItem, RoomPoliciesRequest,
    RoomUsersAddBatchRequestItem,
};
use dco3::{auth::Connected, Dracoon};
use dco3::{Groups, Rooms, Users};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::hash::Hash;

use regex::Regex;

use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::cmd::models::DcCmdError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupRoomPermission {
    pub id: u64,
    pub new_group_member_acceptance: Option<GroupMemberAcceptance>,
    pub permissions: NodePermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRoomPermission {
    pub id: u64,
    pub permissions: NodePermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomPolicies {
    pub default_expiration_period: Option<u64>,
    pub is_virus_protection_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)] // This will make the compiler error if there are unknown fields in the JSON which could be typos and thus result in None values
pub struct Room {
    pub name: String,
    pub recycle_bin_retention_period: Option<u32>,
    pub quota: Option<u64>,
    pub inherit_permissions: Option<bool>,
    pub admin_ids: Option<Vec<u64>>,
    pub admin_group_ids: Option<Vec<u64>>,
    pub user_permissions: Option<Vec<UserRoomPermission>>,
    pub group_permissions: Option<Vec<GroupRoomPermission>>,
    pub new_group_member_acceptance: Option<GroupMemberAcceptance>,
    pub classification: Option<u8>,
    pub policies: Option<RoomPolicies>,
    pub sub_rooms: Option<Vec<Room>>,
}

struct HeaderIndexes {
    name_index: Option<usize>,
    user_id_index: Option<usize>,
    user_permissions_index: Option<usize>,
    group_id_index: Option<usize>,
    new_group_member_acceptance_index: Option<usize>,
    group_permissions_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomImport {
    pub total_room_count: u64,
    pub rooms: Vec<Room>,
}

pub struct UpdateTask {
    pub(crate) task_type: UpdateRoomTaskType,
}
pub enum UpdateRoomTaskType {
    Group(RoomId, Vec<RoomGroupsAddBatchRequestItem>),
    User(RoomId, Vec<RoomUsersAddBatchRequestItem>),
    Policies(RoomId, RoomPoliciesRequest),
}

pub struct UpdateTasksChannel {
    ///
    /// We first create rooms and then send tasks to be executed later
    /// Tasks are send in update_room_groups(), update_room_users() and update_room_policies() after a room was created
    /// The tasks are then collected and executed in the collect_than_complete() function after all rooms were created
    ///
    rx: mpsc::Receiver<UpdateTask>,
    tx: mpsc::Sender<UpdateTask>,
    pub room_group_tasks: RoomGroupTask,
    pub room_user_tasks: RoomUserTask,
    pub room_policies_tasks: RoomPoliciesTask,
}

#[derive(Debug, Clone)]
pub struct RoomGroupTask(Vec<(RoomId, Vec<RoomGroupsAddBatchRequestItem>)>);

#[derive(Debug, Clone)]
pub struct RoomUserTask(Vec<(RoomId, Vec<RoomUsersAddBatchRequestItem>)>);

#[derive(Debug, Clone)]
pub struct RoomPoliciesTask(Vec<(RoomId, RoomPoliciesRequest)>);

static CAPACITY: usize = 50000;

impl UpdateTasksChannel {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(CAPACITY);

        Self {
            rx,
            tx,
            room_group_tasks: RoomGroupTask(Vec::new()),
            room_user_tasks: RoomUserTask(Vec::new()),
            room_policies_tasks: RoomPoliciesTask(Vec::new()),
        }
    }

    pub fn get_sender(&self) -> mpsc::Sender<UpdateTask> {
        self.tx.clone()
    }

    pub fn get_stats(&self) -> usize {
        debug!("Current capacity: {}", self.tx.capacity());
        self.tx.capacity()
    }

    pub async fn collect_than_complete(&mut self, term: Term, dracoon: &Dracoon<Connected>) {
        self.collect_tasks().await;

        // Shutdown the reciever
        self.shutdown();

        self.complete_tasks(dracoon, term).await;
    }

    /// pub only for testing
    pub async fn collect_tasks(&mut self) {
        if self.tx.capacity() == CAPACITY {
            debug!("No tasks to collect.");
            return;
        }

        while let Some(task) = self.rx.recv().await {
            let capacity = self.get_stats();

            match task.task_type {
                UpdateRoomTaskType::Group(room_id, groups) => {
                    debug!("Recieved RoomGroup task for room: {}", room_id.0);
                    self.room_group_tasks.0.push((room_id, groups));
                }
                UpdateRoomTaskType::User(room_id, users) => {
                    debug!("Recieved RoomUser task for room: {}", room_id.0);
                    self.room_user_tasks.0.push((room_id, users));
                }
                UpdateRoomTaskType::Policies(room_id, policies) => {
                    debug!("Recieved RoomPolicies task for room: {}", room_id.0);
                    self.room_policies_tasks.0.push((room_id, policies));
                }
            }

            if capacity == CAPACITY {
                info!("All tasks collected. Closing channel.");
                break;
            }
        }
    }

    async fn complete_tasks(&mut self, dracoon: &Dracoon<Connected>, term: Term) {
        self.complete_room_group_tasks(self.room_group_tasks.clone(), dracoon, term.clone())
            .await;

        self.complete_room_users_tasks(self.room_user_tasks.clone(), dracoon, term.clone())
            .await;

        self.complete_room_policies_tasks(self.room_policies_tasks.clone(), dracoon, term.clone())
            .await;
    }

    async fn complete_room_group_tasks(
        &self,
        room_group_tasks: RoomGroupTask,
        dracoon: &Dracoon<Connected>,
        term: Term,
    ) {
        let total_size = room_group_tasks.0.len();
        let progress_bar = ProgressBar::new(total_size as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
                )
                .unwrap()
                .progress_chars("=>-"),
        );

        progress_bar.set_message("Updating group permissions for rooms");

        for (room_id, groups) in room_group_tasks.0.iter() {
            let res = dracoon
                .nodes
                .update_room_groups(room_id.clone().0, groups.clone().into())
                .await;

            match res {
                Ok(_) => {
                    progress_bar.inc(1);
                    debug!("Updated group permissions for room: {}", room_id.clone().0);
                }
                Err(e) => {
                    error!("Error updating group permissions for room: {}", e);
                    term.write_line(&std::format!(
                        "Error updating group permissions for room: {}",
                        e
                    ))
                    .expect("Error writing message to terminal.");
                }
            }
        }
        if room_group_tasks.0.is_empty() {
            progress_bar.finish_with_message("No group permissions to update.");
        } else {
            progress_bar.finish_with_message(format!(
                "Sucessfully updated {total_size} group permission(s)"
            ));
        }
    }

    async fn complete_room_users_tasks(
        &self,
        room_user_tasks: RoomUserTask,
        dracoon: &Dracoon<Connected>,
        term: Term,
    ) {
        let total_size = room_user_tasks.0.len();
        let progress_bar = ProgressBar::new(total_size as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
                )
                .unwrap()
                .progress_chars("=>-"),
        );
        progress_bar.set_message("Updating user permissions for rooms");

        for (room_id, users) in room_user_tasks.0.iter() {
            let res = dracoon
                .nodes
                .update_room_users(room_id.clone().0, users.clone().into())
                .await;

            match res {
                Ok(_) => {
                    progress_bar.inc(1);
                    debug!("Added users to room: {}", room_id.clone().0);
                }
                Err(e) => {
                    error!("Error adding users to room: {}", e);
                    term.write_line(&std::format!(
                        "Error adding users to room: {}",
                        room_id.clone().0
                    ))
                    .expect("Error writing message to terminal.");
                }
            }
        }
        if room_user_tasks.0.is_empty() {
            progress_bar.finish_with_message("No user permissions to update.");
        } else {
            progress_bar.finish_with_message(format!(
                "Successfully updated {total_size} user permission(s)."
            ));
        }
    }

    async fn complete_room_policies_tasks(
        &self,
        room_policies_tasks: RoomPoliciesTask,
        dracoon: &Dracoon<Connected>,
        term: Term,
    ) {
        let total_size = room_policies_tasks.0.len();
        let progress_bar = ProgressBar::new(total_size as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
                )
                .unwrap()
                .progress_chars("=>-"),
        );

        progress_bar.set_message("Updating policies for rooms");

        for (room_id, policies) in room_policies_tasks.0.iter() {
            let res = dracoon
                .nodes
                .update_room_policies(room_id.clone().0, policies.clone())
                .await;

            match res {
                Ok(_) => {
                    progress_bar.set_message(std::format!(
                        "Updated room policies for room: {}",
                        room_id.clone().0
                    ));
                    progress_bar.inc(1);
                    debug!("Updated room policies for room: {}", room_id.clone().0);
                }
                Err(e) => {
                    error!("Error updating room policies for room: {}", e);
                    term.write_line(&std::format!(
                        "Error updating room policies for room: {}",
                        e
                    ))
                    .expect("Error writing message to terminal.");
                }
            }
        }

        if room_policies_tasks.0.is_empty() {
            progress_bar.finish_with_message("No room policies to update.");
        } else if room_policies_tasks.0.len() == 1 {
            progress_bar
                .finish_with_message(format!("Sucessfully updated {total_size} room policy."));
        } else {
            progress_bar
                .finish_with_message(format!("Sucessfully updated {total_size} room policies."));
        }
    }

    pub fn shutdown(&mut self) {
        debug!("Shutting down reciever for UpdateTasks.");
        self.rx.close();
    }

    // used for testing
    #[allow(unused)]
    pub fn get_policy_task_count(&self) -> usize {
        self.room_policies_tasks.0.len()
    }
}

unsafe impl Send for UpdateTasksChannel {
    // This is safe because the only way to access the tasks is through the public API
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoomId(pub u64);

impl Room {
    fn check_self_and_sub_rooms_for_admin(&self) -> Result<(), DcCmdError> {
        self.has_admin()?;

        if let Some(sub_rooms) = &self.sub_rooms {
            sub_rooms
                .iter()
                .try_for_each(|room| room.check_self_and_sub_rooms_for_admin())?;
        }

        Ok(())
    }

    fn has_admin(&self) -> Result<(), DcCmdError> {
        let has_admin = self.has_admin_ids()
            || self.has_group_admin_ids()
            || self.has_user_permissions_with_manage()
            || self.has_group_permissions_with_manage()
            || self.inherit_permissions.unwrap_or(false);

        match has_admin {
            true => Ok(()),
            false => Err(DcCmdError::ImportedRoomHasNoAdmin(format!(
                "Room '{}' does not have an admin.",
                self.name
            ))),
        }
    }

    fn has_admin_ids(&self) -> bool {
        self.admin_ids.is_some() && !self.admin_ids.as_ref().unwrap().is_empty()
    }

    fn has_group_admin_ids(&self) -> bool {
        self.admin_group_ids.is_some() && !self.admin_group_ids.as_ref().unwrap().is_empty()
    }

    fn has_user_permissions_with_manage(&self) -> bool {
        self.user_permissions
            .as_ref()
            .map(|perms| perms.iter().any(|user| user.permissions.manage))
            .unwrap_or(false)
    }

    fn has_group_permissions_with_manage(&self) -> bool {
        self.group_permissions
            .as_ref()
            .map(|perms| perms.iter().any(|group| group.permissions.manage))
            .unwrap_or(false)
    }

    fn check_self_and_sub_rooms_for_conflicting_permissions(&self) -> Result<(), DcCmdError> {
        // check if one of the admin ids is within the user permissions and has not manage permission
        if let Some(admin_ids) = &self.admin_ids {
            if let Some(user_permissions) = &self.user_permissions {
                if user_permissions
                    .iter()
                    .any(|user| admin_ids.contains(&user.id) && !user.permissions.manage)
                {
                    return Err(DcCmdError::ConflictingRoomPermissions(format!(
                        "Room '{}' has conflicting permissions. User with ID '{}' is an admin and has conflicting user permissions. Remove the user permission",
                        self.name,
                        user_permissions
                            .iter()
                            .find(|user| admin_ids.contains(&user.id))
                            .unwrap()
                            .id
                    )));
                }
            }
        }

        if let Some(sub_rooms) = &self.sub_rooms {
            sub_rooms
                .iter()
                .try_for_each(|room| room.check_self_and_sub_rooms_for_conflicting_permissions())?;
        }

        Ok(())
    }

    fn check_self_and_sub_rooms_for_illegal_room_names(&self) -> Result<(), DcCmdError> {
        self.check_for_illegal_characters()?;
        self.check_room_name_too_long()?;
        self.check_room_name_begins_with_hyphen()?;
        self.check_room_name_ends_with_period()?;

        if let Some(sub_rooms) = &self.sub_rooms {
            sub_rooms
                .iter()
                .try_for_each(|room| room.check_self_and_sub_rooms_for_illegal_room_names())?;
        }

        Ok(())
    }

    fn check_for_illegal_characters(&self) -> Result<(), DcCmdError> {
        let illegal_characters = ['/', '\\', '?', '%', '*', ':', '|', '"', '<', '>', '.'];

        if self.name.chars().any(|c| illegal_characters.contains(&c)) {
            return Err(DcCmdError::IllegalRoomName(format!(
                "Room '{}' name contains illegal character.",
                self.name
            )));
        }

        Ok(())
    }

    fn check_room_name_too_long(&self) -> Result<(), DcCmdError> {
        if self.name.len() > 255 {
            return Err(DcCmdError::IllegalRoomName(format!(
                "Room '{}' name is too long.",
                self.name
            )));
        }

        Ok(())
    }

    fn check_room_name_begins_with_hyphen(&self) -> Result<(), DcCmdError> {
        if self.name.starts_with('-') {
            return Err(DcCmdError::IllegalRoomName(format!(
                "Room '{}' name begins with a hyphen.",
                self.name
            )));
        }

        Ok(())
    }

    fn check_room_name_ends_with_period(&self) -> Result<(), DcCmdError> {
        if self.name.ends_with('.') {
            return Err(DcCmdError::IllegalRoomName(format!(
                "Room '{}' name ends with a period.",
                self.name
            )));
        }

        Ok(())
    }
}

impl RoomImport {
    pub async fn from_path(
        path: String,
        template_filler_path: Option<String>,
        dracoon: &Dracoon<Connected>,
        term: Term,
    ) -> Result<Self, DcCmdError> {
        let room_struct = Self::read_and_parse_room_data(path, template_filler_path, term.clone())?;

        Self::analyize_and_validate_room_import_data(term, room_struct, dracoon).await
    }

    fn read_and_parse_room_data(
        path: String,
        template_filler_path: Option<String>,
        term: Term,
    ) -> Result<Vec<Room>, DcCmdError> {
        let data = std::fs::read_to_string(path).map_err(|e| {
            error!("Failed to read file: {}", e);
            DcCmdError::IoError
        })?;

        match template_filler_path {
            Some(template_filler_path) => Ok(Self::construct_rooms_from_template_files(
                template_filler_path,
                data,
                term.clone(),
            )?),
            None => Ok(serde_json::from_str(&data).expect("JSON does not have correct format.")),
        }
    }

    async fn analyize_and_validate_room_import_data(
        term: Term,
        room_struct: Vec<Room>,
        dracoon: &Dracoon<Connected>,
    ) -> Result<RoomImport, DcCmdError> {
        info!("Validating JSON file.");
        term.write_line("Validating JSON file.")
            .expect("Error writing message to terminal.");
        for room in &room_struct {
            room.check_self_and_sub_rooms_for_admin()?;
            room.check_self_and_sub_rooms_for_illegal_room_names()?;
            room.check_self_and_sub_rooms_for_conflicting_permissions()?;
        }

        Self::check_if_virus_protection_is_some(room_struct.clone());

        // todo impl function in dco3 and check if virus protection is enabled when virus_protection_policy_found is true

        info!("Checking if user and group ids in JSON file exist.");
        term.write_line("Checking if user and group ids in JSON file exist.")
            .expect("Error writing message to terminal.");

        let total_room_count = Self::get_total_rooms(room_struct.clone());

        Self::check_user_and_group_existence(
            Self {
                total_room_count,
                rooms: room_struct.clone(),
            },
            dracoon,
        )
        .await?;

        info!("Checks completed successfully.");
        term.write_line("Checks completed successfully.")
            .expect("Error writing message to terminal.");

        Ok(Self {
            total_room_count,
            rooms: room_struct,
        })
    }

    fn get_total_rooms(room_struct: Vec<Room>) -> u64 {
        let mut count = 0;
        for room in room_struct {
            count += 1;
            if let Some(sub_rooms) = room.sub_rooms {
                count += Self::get_total_rooms(sub_rooms);
            }
        }
        count
    }

    fn get_all_unique_user_ids(room_struct: &Vec<Room>) -> Vec<u64> {
        let mut user_ids = Vec::new();
        for room in room_struct {
            if let Some(admin_ids) = &room.admin_ids {
                for admin_id in admin_ids {
                    if !user_ids.contains(admin_id) {
                        user_ids.push(*admin_id);
                    }
                }
            }
            if let Some(user_permissions) = &room.user_permissions {
                for user in user_permissions {
                    if !user_ids.contains(&user.id) {
                        user_ids.push(user.id);
                    }
                }
            }
            if let Some(sub_rooms) = &room.sub_rooms {
                user_ids.append(&mut Self::get_all_unique_user_ids(sub_rooms));
            }
        }
        user_ids.sort();
        user_ids.dedup();
        user_ids
    }

    fn get_all_unique_group_ids(room_struct: &Vec<Room>) -> Vec<u64> {
        let mut group_ids = Vec::new();
        for room in room_struct {
            if let Some(admin_group_ids) = &room.admin_group_ids {
                for admin_group_id in admin_group_ids {
                    if !group_ids.contains(admin_group_id) {
                        group_ids.push(*admin_group_id);
                    }
                }
            }
            if let Some(group_permissions) = &room.group_permissions {
                for group in group_permissions {
                    if !group_ids.contains(&group.id) {
                        group_ids.push(group.id);
                    }
                }
            }
            if let Some(sub_rooms) = &room.sub_rooms {
                group_ids.append(&mut Self::get_all_unique_group_ids(sub_rooms));
            }
        }
        group_ids.sort();
        group_ids.dedup();
        group_ids
    }

    async fn check_user_and_group_existence(
        self,
        dracoon: &Dracoon<Connected>,
    ) -> Result<(), DcCmdError> {
        self.check_if_user_exists(dracoon).await?;
        self.check_if_group_exists(dracoon).await?;
        Ok(())
    }

    async fn check_if_group_exists(&self, dracoon: &Dracoon<Connected>) -> Result<(), DcCmdError> {
        let group_ids = Self::get_all_unique_group_ids(self.rooms.as_ref());
        for group_id in group_ids {
            let res = dracoon.groups.get_group(group_id).await;
            match res {
                Ok(_) => {}
                Err(_) => {
                    return Err(DcCmdError::GroupNotFound(format!(
                        "Group with ID '{}' does not exist.",
                        group_id
                    )));
                }
            }
        }
        Ok(())
    }

    async fn check_if_user_exists(&self, dracoon: &Dracoon<Connected>) -> Result<(), DcCmdError> {
        let user_ids = Self::get_all_unique_user_ids(self.rooms.as_ref());
        for user in user_ids {
            let res = dracoon.users.get_user(user, None).await;
            match res {
                Ok(_) => {}
                Err(_) => {
                    return Err(DcCmdError::UserDoesNotExist(format!(
                        "User with ID '{}' does not exist.",
                        user
                    )));
                }
            }
        }
        Ok(())
    }

    fn check_if_virus_protection_is_some(rooms: Vec<Room>) -> bool {
        rooms.iter().any(|room| {
            room.policies.as_ref().map_or(false, |policies| {
                policies.is_virus_protection_enabled.is_some()
            }) || room.sub_rooms.as_ref().map_or(false, |sub_rooms| {
                Self::check_if_virus_protection_is_some(sub_rooms.clone())
            })
        })
    }

    fn parse_group_member_acceptance(field: &str) -> Option<GroupMemberAcceptance> {
        match field {
            "autoallow" => Some(GroupMemberAcceptance::AutoAllow),
            "pending" => Some(GroupMemberAcceptance::Pending),
            _ => None,
        }
    }

    fn construct_rooms_from_template_files(
        template_filler_path: String,
        template_content: String,
        term: Term,
    ) -> Result<Vec<Room>, DcCmdError> {
        let tokens = Self::get_template_tokens(&template_content, term.clone())?;
        let rooms_map = Self::read_file_and_construct_room_map(template_filler_path, tokens)?;
        Self::fill_template_with_data(rooms_map, template_content)
    }

    fn get_template_tokens(template_content: &str, term: Term) -> Result<Vec<String>, DcCmdError> {
        // write a regex to find all template tokens (characters within {{ and }}) in the template_content
        // if no tokens are found, return an error
        // if tokens are found, return the tokens

        let re = Regex::new(r"\{\{(.*?)\}\}").unwrap();
        let tokens: Vec<String> = re
            .captures_iter(template_content)
            .map(|cap| cap[1].trim().to_string())
            .collect();

        if tokens.is_empty() {
            error!("No template tokens found in template content.");
            term.write_line("No template tokens found in template content.")
                .expect("Error writing message to terminal.");
            return Err(DcCmdError::NoTemplateTokensFound(
                "No template tokens found in template content.".to_string(),
            ));
        }

        // check if there are other tokens then "name", "userPermissions", "groupPermissions" and throw an error if there are
        let allowed_tokens = ["name", "userPermissions", "groupPermissions"];
        let invalid_tokens: Vec<String> = tokens
            .iter()
            .filter(|token| !allowed_tokens.contains(&token.as_str()))
            .cloned()
            .collect();

        if !invalid_tokens.is_empty() {
            error!(
                "Invalid template tokens found in template content: {:?}. Only '{{ name }}', '{{ userPermissions }}' and '{{ groupPermissions }}' are allowed.",
                invalid_tokens
            );
            return Err(DcCmdError::InvalidTemplateTokens(format!(
                "Invalid template tokens found in template content: {:?}. Only '{{ name }}', '{{ userPermissions }}' and '{{ groupPermissions }}' are allowed.",
                invalid_tokens
            )));
        }

        Ok(tokens)
    }

    fn read_file_and_construct_room_map(
        template_filler_path: String,
        tokens: Vec<String>,
    ) -> Result<HashMap<String, Room>, DcCmdError> {
        let file = File::open(template_filler_path).map_err(|e| {
            error!("Failed to open file: {}", e);
            DcCmdError::IoError
        })?;

        let mut csv_data = Reader::from_reader(file);

        // Find the indexes of the columns dynamically
        let header_indexes = Self::get_header_indexes(&mut csv_data)?;
        Self::validate_tokens_against_headers(&header_indexes, tokens)?;

        let mut rooms_map: HashMap<String, Room> = HashMap::new();

        for result in csv_data.records() {
            let record = result.map_err(|e| {
                error!("Failed to read CSV record: {}", e);
                DcCmdError::CsvReadRecord(format!("Failed to read CSV record: {}", e))
            })?;

            let name = header_indexes
                .name_index
                .and_then(|idx| record.get(idx))
                .unwrap_or("")
                .to_string();
            let user_id: Option<u64> = header_indexes
                .user_id_index
                .and_then(|idx| record.get(idx))
                .and_then(|s| s.parse().ok());
            let user_permissions: Option<NodePermissions> = header_indexes
                .user_permissions_index
                .and_then(|idx| record.get(idx))
                .and_then(|s| serde_json::from_str(s).ok());
            let group_id: Option<u64> = header_indexes
                .group_id_index
                .and_then(|idx| record.get(idx))
                .and_then(|s| s.parse().ok());
            let new_group_member_acceptance: Option<GroupMemberAcceptance> = header_indexes
                .new_group_member_acceptance_index
                .and_then(|idx| record.get(idx))
                .and_then(Self::parse_group_member_acceptance);
            let group_permissions: Option<NodePermissions> = header_indexes
                .group_permissions_index
                .and_then(|idx| record.get(idx))
                .and_then(|s| serde_json::from_str(s).ok());

            let room = rooms_map.entry(name.clone()).or_insert_with(|| Room {
                name: name.clone(),
                ..Default::default()
            });

            if let Some(uid) = user_id {
                if let Some(uperm) = user_permissions {
                    let user_permission = UserRoomPermission {
                        id: uid,
                        permissions: uperm,
                    };

                    room.user_permissions
                        .get_or_insert_with(Vec::new)
                        .push(user_permission);
                }
            }

            if let Some(gid) = group_id {
                if let Some(gperm) = group_permissions {
                    let group_permission = GroupRoomPermission {
                        id: gid,
                        new_group_member_acceptance: new_group_member_acceptance.clone(),
                        permissions: gperm,
                    };

                    room.group_permissions
                        .get_or_insert_with(Vec::new)
                        .push(group_permission);
                }
            }
        }
        Ok(rooms_map)
    }

    fn get_header_indexes(csv_data: &mut Reader<File>) -> Result<HeaderIndexes, DcCmdError> {
        let headers = csv_data
            .headers()
            .map_err(|e| {
                error!("Failed to read CSV headers: {}", e);
                DcCmdError::CsvReadHeaders(format!("Failed to read CSV headers: {}", e))
            })?
            .clone();

        let name_index = headers.iter().position(|h| h == "name");

        if name_index.is_none() {
            error!("No 'name' column found in CSV file.");
            return Err(DcCmdError::CsvReadHeaders(
                "No 'name' column found in CSV file.".to_string(),
            ));
        }

        let user_id_index = headers.iter().position(|h| h == "userId");
        let user_permissions_index = headers.iter().position(|h| h == "userPermissions");

        if user_id_index.is_some() && user_permissions_index.is_none() {
            error!("'userId' column found but no 'userPermissions' column found in CSV file.");
            return Err(DcCmdError::CsvReadHeaders(
                "'userId' column found but no 'userPermissions' column found in CSV file."
                    .to_string(),
            ));
        }

        let group_id_index = headers.iter().position(|h| h == "groupId");
        let new_group_member_acceptance_index =
            headers.iter().position(|h| h == "newGroupMemberAcceptance");
        let group_permissions_index = headers.iter().position(|h| h == "groupPermissions");

        if group_id_index.is_some() && group_permissions_index.is_none() {
            error!("'groupId' column found but no 'groupPermissions' column found in CSV file.");
            return Err(DcCmdError::CsvReadHeaders(
                "'groupId' column found but no 'groupPermissions' column found in CSV file."
                    .to_string(),
            ));
        }

        Ok(HeaderIndexes {
            name_index,
            user_id_index,
            user_permissions_index,
            group_id_index,
            new_group_member_acceptance_index,
            group_permissions_index,
        })
    }

    fn validate_tokens_against_headers(
        header_indexes: &HeaderIndexes,
        tokens: Vec<String>,
    ) -> Result<(), DcCmdError> {
        if tokens.iter().any(|token| token == "userPermission")
            && (header_indexes.user_id_index.is_none()
                || header_indexes.user_permissions_index.is_none())
        {
            error!("'userPermissions' token found but no 'userId' or 'userPermissions' column found in CSV file.");
            return Err(DcCmdError::TemplateTokenConflict(
                "'userPermissions' token found but no 'userId' or 'userPermissions' column found in CSV file.".to_string(),
            ));
        }

        if tokens.iter().any(|token| token == "groupPermissions")
            && (header_indexes.group_id_index.is_none()
                || header_indexes.group_permissions_index.is_none())
        {
            error!("'groupPermissions' token found in template but no 'groupId' or 'groupPermissions' column found in CSV file.");
            return Err(DcCmdError::TemplateTokenConflict(
           "'groupPermissions' token found in template but no 'groupId' or 'groupPermissions' column found in CSV file.".to_string(),
            ));
        }

        Ok(())
    }

    fn fill_template_with_data(
        rooms_map: HashMap<String, Room>,
        template_content: String,
    ) -> Result<Vec<Room>, DcCmdError> {
        let mut templated_rooms = Vec::new();

        for (_, room) in rooms_map {
            let mut template_content = template_content.clone();

            template_content = template_content.replace(
                "\"{{ name }}\"",
                serde_json::to_string(&room.name)
                    .map_err(|e| {
                        error!("Failed to serialize room name: {}", e);
                        DcCmdError::SerdeSerializeToString(format!(
                            "Failed to serialize room name: {}",
                            e
                        ))
                    })?
                    .as_str(),
            );

            if let Some(ref user_permissions) = room.user_permissions {
                template_content = template_content.replace(
                    // escape " to prevent JSON parsing errors"
                    "\"{{ userPermissions }}\"",
                    serde_json::to_string(user_permissions)
                        .map_err(|e| {
                            error!("Failed to serialize user permissions: {}", e);
                            DcCmdError::SerdeSerializeToString(format!(
                                "Failed to serialize user permissions: {}",
                                e
                            ))
                        })?
                        .as_str(),
                );
            }
            if let Some(ref group_permissions) = room.group_permissions {
                template_content = template_content.replace(
                    "\"{{ groupPermissions }}\"",
                    serde_json::to_string(group_permissions)
                        .map_err(|e| {
                            error!("Failed to serialize group permissions: {}", e);
                            DcCmdError::SerdeSerializeToString(format!(
                                "Failed to serialize group permissions: {}",
                                e
                            ))
                        })?
                        .to_owned()
                        .as_str(),
                );
            }
            // Deserialize the JSON string to a Room struct
            let filled_room: Room = serde_json::from_str(template_content.as_str()).map_err(|e| {
                eprintln!("Failed to parse JSON: {}. Check if JSON template is an object and not an array.", e);
                e
            }).map_err(|e| {
                error!("Failed to deserialize JSON template: {}", e);
                DcCmdError::JsonParseTemplate(format!(
                    "Failed to deserialize JSON template: {}",
                    e
                ))
            })?;

            templated_rooms.push(filled_room);
        }

        Ok(templated_rooms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_template_tokens() {
        let template_content = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tests/get_template_tokens_all_tokens.json"
        ));

        let tokens =
            RoomImport::get_template_tokens(&template_content.to_string(), console::Term::stdout())
                .expect("Failed to get template tokens.");

        assert_eq!(tokens, vec!["name", "userPermissions", "groupPermissions"]);
    }

    #[test]
    fn test_get_template_tokens_wrong_token() {
        let template_content = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tests/get_template_tokens_wrong_token.json"
        ));

        let tokens =
            RoomImport::get_template_tokens(&template_content.to_string(), console::Term::stdout());

        assert!(tokens.is_err());
        assert!(matches!(
            tokens.unwrap_err(),
            DcCmdError::InvalidTemplateTokens(_)
        ));
    }

    #[test]
    fn test_get_template_tokens_no_token() {
        let template_content = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tests/get_template_tokens_no_token.json"
        ));

        let tokens =
            RoomImport::get_template_tokens(&template_content.to_string(), console::Term::stdout());

        assert!(tokens.is_err());
        assert!(matches!(
            tokens.unwrap_err(),
            DcCmdError::NoTemplateTokensFound(_)
        ));
    }

    #[test]
    fn test_fill_template_with_data() {
        let template_filler_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tests/fill_template_with_data.csv"
        );

        let template_content = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tests/fill_template_with_data_template.json"
        ));

        let tokens =
            RoomImport::get_template_tokens(&template_content.to_string(), console::Term::stdout())
                .expect("Failed to get template tokens.");

        println!("{:?}", template_filler_path);

        let rooms_map =
            RoomImport::read_file_and_construct_room_map(template_filler_path.to_string(), tokens)
                .expect("Failed to read file and construct room map.");

        let mut filled_rooms =
            RoomImport::fill_template_with_data(rooms_map, template_content.to_string())
                .expect("Failed to fill template with data.");

        filled_rooms.sort_by(|a, b| a.name.cmp(&b.name));

        let room_1 = filled_rooms[0].clone();

        assert_eq!(filled_rooms.len(), 3);
        assert_eq!(room_1.clone().name, "test");
        assert_eq!(room_1.clone().user_permissions.unwrap()[0].id, 2);
        assert_eq!(
            room_1.clone().user_permissions.unwrap()[0]
                .permissions
                .manage,
            true
        );
        assert_eq!(room_1.clone().group_permissions.unwrap()[0].id, 4);
        assert_eq!(
            room_1.clone().group_permissions.unwrap()[0]
                .permissions
                .manage,
            true
        );
        assert_eq!(
            room_1.clone().group_permissions.unwrap()[0].new_group_member_acceptance,
            Some(GroupMemberAcceptance::AutoAllow)
        );
        assert_eq!(room_1.clone().user_permissions.unwrap().len(), 2);

        assert_eq!(filled_rooms[1].name, "test_2");
        assert_eq!(filled_rooms[2].name, "test_3");
    }

    #[test]
    fn test_read_file_and_construct_room_map() {
        let template_filler_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tests/fill_template_with_data.csv"
        );

        let template_content = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tests/fill_template_with_data_template.json"
        ));

        let tokens =
            RoomImport::get_template_tokens(&template_content.to_string(), console::Term::stdout())
                .expect("Failed to get template tokens.");

        let rooms_map =
            RoomImport::read_file_and_construct_room_map(template_filler_path.to_string(), tokens)
                .expect("Failed to read file and construct room map.");

        let room_1 = rooms_map.get("test").unwrap().clone();
        assert!(room_1.name == "test");
        assert!(room_1.user_permissions.is_some());
        assert!(room_1.group_permissions.is_some());
        assert!(room_1.user_permissions.unwrap().len() == 2);
        assert!(room_1.group_permissions.clone().unwrap().len() == 2);
        assert!(&room_1.group_permissions.clone().unwrap()[0]
            .new_group_member_acceptance
            .is_some());
        assert!(
            room_1.group_permissions.clone().unwrap()[0]
                .new_group_member_acceptance
                .clone()
                .unwrap()
                == GroupMemberAcceptance::AutoAllow
        );
        let room_2 = rooms_map.get("test_2").unwrap().clone();
        assert!(room_2.name == "test_2");
    }

    #[test]
    fn test_get_header_indexes() {
        let template_filler_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tests/fill_template_with_data.csv"
        );

        let file = File::open(template_filler_path).expect("Failed to open file.");

        let mut csv_data = Reader::from_reader(file);

        let header_indexes =
            RoomImport::get_header_indexes(&mut csv_data).expect("Failed to get header indexes.");

        assert_eq!(header_indexes.name_index, Some(0));
        assert_eq!(header_indexes.user_id_index, Some(1));
        assert_eq!(header_indexes.user_permissions_index, Some(2));
        assert_eq!(header_indexes.group_id_index, Some(3));
        assert_eq!(header_indexes.new_group_member_acceptance_index, Some(4));
        assert_eq!(header_indexes.group_permissions_index, Some(5));
    }
}
