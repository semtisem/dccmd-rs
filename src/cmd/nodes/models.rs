use std::fs::File;

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
use tera::{Context, Tera};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::cmd::{init_dracoon, models::DcCmdError};

pub struct UploadOptions {
    pub overwrite: bool,
    pub classification: Option<u8>,
    pub velocity: Option<u8>,
    pub recursive: bool,
    pub skip_root: bool,
    pub share: bool,
}

impl UploadOptions {
    pub fn new(
        overwrite: bool,
        classification: Option<u8>,
        velocity: Option<u8>,
        recursive: bool,
        skip_root: bool,
        share: bool,
    ) -> Self {
        Self {
            overwrite,
            classification,
            velocity,
            recursive,
            skip_root,
            share,
        }
    }
}

pub struct UploadCommandHandler {
    client: Dracoon<Connected>,
    term: Term,
}

impl UploadCommandHandler {
    pub async fn try_new(target_domain: &str, term: Term) -> Result<Self, DcCmdError> {
        let client = init_dracoon(target_domain, None, false).await?;
        Ok(Self { client, term })
    }

    pub fn client(&self) -> &Dracoon<Connected> {
        &self.client
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomImport {
    pub total_room_count: u64,
    pub rooms: Vec<Room>,
}

pub struct UpdateTask {
    pub(crate) task_type: UpdateTaskType,
}
pub enum UpdateTaskType {
    RoomGroup(RoomId, Vec<RoomGroupsAddBatchRequestItem>),
    RoomUser(RoomId, Vec<RoomUsersAddBatchRequestItem>),
    RoomPolicies(RoomId, RoomPoliciesRequest),
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

    pub fn get_stats(&self, term: Term) -> usize {
        debug!("Current capacity: {}", self.tx.capacity());
        self.tx.capacity()
    }

    pub async fn collect_than_complete(&mut self, term: Term, dracoon: &Dracoon<Connected>) {
        self.collect_tasks(term.clone()).await;

        // Shutdown the reciever
        self.shutdown(term.clone());

        self.complete_tasks(dracoon, term).await;
    }

    /// pub only for testing
    pub async fn collect_tasks(&mut self, term: Term) {
        if self.tx.capacity() == CAPACITY {
            debug!("No tasks to collect.");
            return;
        }

        while let Some(task) = self.rx.recv().await {
            let capacity = self.get_stats(term.clone());

            match task.task_type {
                UpdateTaskType::RoomGroup(room_id, groups) => {
                    debug!("Recieved RoomGroup task for room: {}", room_id.0);
                    self.room_group_tasks.0.push((room_id, groups));
                }
                UpdateTaskType::RoomUser(room_id, users) => {
                    debug!("Recieved RoomUser task for room: {}", room_id.0);
                    self.room_user_tasks.0.push((room_id, users));
                }
                UpdateTaskType::RoomPolicies(room_id, policies) => {
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
                    term.write_line(&std::format!(
                        "Updated group permissions for room: {}",
                        room_id.clone().0
                    ))
                    .expect("Error writing message to terminal.");
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

    pub fn shutdown(&mut self, term: Term) {
        debug!("Shutting down reciever for UpdateTasks.");
        self.rx.close();
    }

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
    fn check_all_rooms_have_admin(&self) -> Result<(), DcCmdError> {
        let has_admin = self.admin_ids.is_some() && !self.admin_ids.as_ref().unwrap().is_empty()
            || self.admin_group_ids.is_some() && !self.admin_group_ids.as_ref().unwrap().is_empty()
            || self
                .user_permissions
                .as_ref()
                .map(|perms| perms.iter().any(|user| user.permissions.manage))
                .unwrap_or(false)
            || self
                .group_permissions
                .as_ref()
                .map(|perms| perms.iter().any(|group| group.permissions.manage))
                .unwrap_or(false)
            || self.inherit_permissions.unwrap_or(false);

        if !has_admin {
            return Err(DcCmdError::ImportedRoomHasNoAdmin(format!(
                "Room '{}' does not have an admin and inheritance is disabled.",
                self.name
            )));
        }

        if let Some(sub_rooms) = &self.sub_rooms {
            sub_rooms
                .iter()
                .try_for_each(|room| room.check_all_rooms_have_admin())?;
        }

        Ok(())
    }

    fn check_conflicting_permissions(&self) -> Result<(), DcCmdError> {
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
                .try_for_each(|room| room.check_conflicting_permissions())?;
        }

        Ok(())
    }

    fn check_illegal_characters_in_room_name(&self) -> Result<(), DcCmdError> {
        let illegal_characters = ["\\", "/", ":", "*", "?", "\"", "<", ">", "|"];
        if illegal_characters
            .iter()
            .any(|character| self.name.contains(character))
        {
            return Err(DcCmdError::IllegalRoomName(format!(
                "Room '{}' contains illegal character.",
                self.name
            )));
        }

        if self.name.len() > 255 {
            return Err(DcCmdError::IllegalRoomName(format!(
                "Room '{}' name is too long.",
                self.name
            )));
        }

        if self.name.starts_with('-') {
            return Err(DcCmdError::IllegalRoomName(format!(
                "Room '{}' name begins with a hyphen.",
                self.name
            )));
        }

        if self.name.ends_with('.') {
            return Err(DcCmdError::IllegalRoomName(format!(
                "Room '{}' name ends with a period.",
                self.name
            )));
        }

        if let Some(sub_rooms) = &self.sub_rooms {
            sub_rooms
                .iter()
                .try_for_each(|room| room.check_illegal_characters_in_room_name())?;
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
        let data = std::fs::read_to_string(&path).map_err(|e| {
            error!("Failed to read file: {}", e);
            DcCmdError::IoError
        })?;

        let room_struct: Vec<Room> = match template_filler_path {
            Some(template_filler_path) => {
                Self::fill_template(template_filler_path, data, term.clone())
            }
            None => serde_json::from_str(&data).expect("JSON does not have correct format."),
        };

        // might want to collect errors and return them all at once to fix multiple issues at once
        info!("Validating JSON file.");
        term.write_line(&std::format!("Validating JSON file."))
            .expect("Error writing message to terminal.");
        for room in &room_struct {
            room.check_all_rooms_have_admin()?;
            room.check_illegal_characters_in_room_name()?;
            room.check_conflicting_permissions()?;
        }

        let total_room_count = Self::get_total_rooms(room_struct.clone());

        let virus_protection_policy_found =
            Self::check_if_virus_protection_is_some(room_struct.clone());

        // todo impl function in dco3 and check if virus protection is enabled when virus_protection_policy_found is true

        info!("Checking if user and group ids in JSON file exist.");
        term.write_line(&std::format!(
            "Checking if user and group ids in JSON file exist."
        ))
        .expect("Error writing message to terminal.");

        Self::check_user_and_group_existence(
            Self {
                total_room_count,
                rooms: room_struct.clone(),
            },
            dracoon,
        )
        .await?;

        info!("Checks completed successfully.");
        term.write_line(&std::format!("Checks completed successfully."))
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
                user_ids.append(&mut Self::get_all_unique_user_ids(&sub_rooms));
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
                group_ids.append(&mut Self::get_all_unique_group_ids(&sub_rooms));
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
                Err(e) => {
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
                Err(e) => {
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

    fn fill_template(
        template_filler_path: String,
        template_content: String,
        term: Term,
    ) -> Vec<Room> {
        let file = File::open(template_filler_path).unwrap();
        let mut csv_data = Reader::from_reader(file);

        let headers = csv_data.headers().unwrap().clone();

        let mut rooms = Vec::new();

        for result in csv_data.records() {
            let record = result.unwrap();

            let mut context = Context::new();

            for (i, header) in headers.iter().enumerate() {
                if let Some(value) = record.get(i) {
                    context.insert(header, value);
                }
            }

            let tera = Tera::one_off(&template_content, &context, true).unwrap();

            // Deserialize the JSON string to a Room struct
            let room: Room = serde_json::from_str(&tera.as_str()).map_err(|e| {
                error!("Failed to parse JSON: {}. Check if JSON template is an object and not an array.", e);
                DcCmdError::JsonParseTemplateError(format!("Failed to parse JSON: {}. Check if JSON template is an object and not an array.", e))
            }).unwrap();

            rooms.push(room);
        }
        dbg!(rooms.clone());
        rooms
    }
}
