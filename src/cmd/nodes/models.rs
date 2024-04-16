use dco3::nodes::{GroupMemberAcceptance, NodePermissions};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::cmd::models::DcCmdError;

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

#[derive(Debug, Serialize, Deserialize)]
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
    pub sub_rooms: Option<Vec<Room>>,
}



impl Room {
    pub fn from_path(path: String,) -> Result<Vec<Self>, DcCmdError> {
        let data = std::fs::read_to_string(&path).map_err(|e| {
            error!("Failed to read file: {}", e);
            DcCmdError::IoError
        })?;
    
        let room_struct: Vec<Room> = serde_json::from_str(&data)
            .expect("JSON does not have correct format.");
        Ok(room_struct)
    }
}


