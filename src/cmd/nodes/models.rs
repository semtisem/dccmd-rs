use console::Term;
use dco3::{auth::Connected, Dracoon};
use dco3::nodes::{GroupMemberAcceptance, NodePermissions};
use serde::{Deserialize, Serialize};
use tracing::error;

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

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)] // This will make the compiler error if there are unknown fields in the JSON which could be typos and thus result in None values
pub struct RoomImport {
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
    pub sub_rooms: Option<Vec<RoomImport>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RoomId(pub u64);


impl RoomImport {
    pub fn from_path(path: String,) -> Result<Vec<Self>, DcCmdError> {
        let data = std::fs::read_to_string(&path).map_err(|e| {
            error!("Failed to read file: {}", e);
            DcCmdError::IoError
        })?;
    
        let room_struct: Vec<RoomImport> = serde_json::from_str(&data)
            .expect("JSON does not have correct format.");

        // might want to collect errors and return them all at once to fix multiple issues at once
        for room in &room_struct {
            room.check_all_rooms_have_admin()?;
            room.check_illegal_characters_in_room_name()?;
        }

        Ok(room_struct)
    }

    fn check_all_rooms_have_admin(&self) -> Result<(), DcCmdError> {
        let has_admin = self.admin_ids.is_some() && !self.admin_ids.as_ref().unwrap().is_empty()
            || self.admin_group_ids.is_some() && !self.admin_group_ids.as_ref().unwrap().is_empty()
            || self.user_permissions.as_ref().map(|perms| perms.iter().any(|user| user.permissions.manage)).unwrap_or(false)
            || self.group_permissions.as_ref().map(|perms| perms.iter().any(|group| group.permissions.manage)).unwrap_or(false);
        //    || self.inherit_permissions.is_some() && self.inherit_permissions.unwrap();

        if !has_admin {
            return Err(DcCmdError::ImportedRoomHasNoAdmin(format!("Room '{}' does not have an admin.", self.name)));
        }

        if let Some(sub_rooms) = &self.sub_rooms {
            for room in sub_rooms {
                room.check_all_rooms_have_admin()?;
            }
        }

        Ok(())
    }

    fn check_illegal_characters_in_room_name(&self) -> Result<(), DcCmdError> {        
        let illegal_characters = vec!["\\", "/", ":", "*", "?", "\"", "<", ">", "|"];
        for character in illegal_characters {
            if self.name.contains(character) {
                return Err(DcCmdError::IllegalRoomName(format!("Room '{}' contains illegal character '{}'.", self.name, character)));
            }
        }

        if self.name.len() > 255 {
            return Err(DcCmdError::IllegalRoomName(format!("Room '{}' name is too long.", self.name)));
        }

        if self.name.starts_with('-') {
            return Err(DcCmdError::IllegalRoomName(format!("Room '{}' name begins with a hyphen.", self.name)));
        }

        if self.name.ends_with('.') {
            return Err(DcCmdError::IllegalRoomName(format!("Room '{}' name ends with a period.", self.name)));
        }

        if let Some(sub_rooms) = &self.sub_rooms {
            for room in sub_rooms {
                room.check_illegal_characters_in_room_name()?;
            }
        }

        Ok(())
    }

}


