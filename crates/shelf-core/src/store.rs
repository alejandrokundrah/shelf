use crate::dpapi;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PairedPeer {
    pub name: String,
    /// Hex of the peer's 32 byte Noise static public key.
    pub public_key: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomRole {
    Owner,
    Member,
    Editor,
    Contributor,
    Viewer,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum RoomUpdate {
    AddAnnouncement { message: String, pinned: bool },
    SetAnnouncementPinned { id: String, pinned: bool },
    AddClipboard { content: String },
    RemoveClipboard { id: String },
    LockFile { hash: String },
    UnlockFile { hash: String },
    SetMemberRole { public_key: String, role: RoomRole },
    AddNote { title: String, body: String },
    AddLink { title: String, url: String },
    SetFileDownloads { hash: String, allowed: bool },
    SetSessionExpiration { expires_at: Option<u64> },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomMember {
    pub name: String,
    pub public_key: String,
    pub role: RoomRole,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomFile {
    pub hash: String,
    pub name: String,
    pub size: u64,
    pub uploaded_by: String,
    pub uploaded_at: u64,
    #[serde(default)]
    pub submission: Option<String>,
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub previous_hash: Option<String>,
    #[serde(default = "default_true")]
    pub downloads_allowed: bool,
    #[serde(default)]
    pub locked_by: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomNote {
    pub id: String,
    pub title: String,
    pub body: String,
    pub updated_by: String,
    pub updated_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomLink {
    pub title: String,
    pub url: String,
    pub added_by: String,
    pub added_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomAnnouncement {
    pub id: String,
    pub message: String,
    pub pinned: bool,
    pub posted_by: String,
    pub posted_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClipboardEntry {
    pub id: String,
    pub content: String,
    pub added_by: String,
    pub added_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ActivityEntry {
    pub message: String,
    pub actor: String,
    pub at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Assignment {
    pub id: String,
    pub title: String,
    pub deadline: u64,
    pub instructions: String,
    pub required_files: Vec<String>,
    pub submissions: Vec<Submission>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Submission {
    pub assignment_id: String,
    pub file_hash: String,
    pub student_public_key: String,
    pub submitted_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomQuestion {
    pub id: String,
    pub question: String,
    pub asked_at: u64,
    pub responses: Vec<QuestionResponse>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QuestionResponse {
    pub member_public_key: String,
    pub answer: String,
    pub answered_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Room {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub invite_code: String,
    pub owner_public_key: String,
    pub members: Vec<RoomMember>,
    #[serde(default)]
    pub files: Vec<RoomFile>,
    #[serde(default)]
    pub assignments: Vec<Assignment>,
    #[serde(default)]
    pub questions: Vec<RoomQuestion>,
    #[serde(default)]
    pub session_expires_at: Option<u64>,
    #[serde(default)]
    pub notes: Vec<RoomNote>,
    #[serde(default)]
    pub links: Vec<RoomLink>,
    #[serde(default)]
    pub announcements: Vec<RoomAnnouncement>,
    #[serde(default)]
    pub clipboard: Vec<ClipboardEntry>,
    #[serde(default)]
    pub activity: Vec<ActivityEntry>,
    #[serde(default)]
    pub pending_members: Vec<RoomMember>,
}

impl Room {
    pub fn can_delete(&self, public_key: &str) -> bool {
        self.owner_public_key == public_key
    }

    pub fn has_member(&self, public_key: &str) -> bool {
        self.members.iter().any(|m| m.public_key == public_key)
    }

    pub fn role_for(&self, public_key: &str) -> Option<RoomRole> {
        self.members
            .iter()
            .find(|member| member.public_key == public_key)
            .map(|member| member.role)
    }

    pub fn can_upload(&self, public_key: &str) -> bool {
        matches!(
            self.role_for(public_key),
            Some(RoomRole::Owner | RoomRole::Editor | RoomRole::Contributor | RoomRole::Member)
        )
    }

    pub fn can_edit(&self, public_key: &str) -> bool {
        matches!(
            self.role_for(public_key),
            Some(RoomRole::Owner | RoomRole::Editor)
        )
    }

    pub fn can_download(&self, public_key: &str, hash: &str) -> bool {
        self.has_member(public_key)
            && self
                .files
                .iter()
                .find(|file| file.hash == hash)
                .map(|file| file.downloads_allowed)
                .unwrap_or(false)
    }

    pub fn lock_file(&mut self, requester: &str, hash: &str) -> bool {
        if !self.can_edit(requester) {
            return false;
        }
        let Some(file) = self.files.iter_mut().find(|file| file.hash == hash) else {
            return false;
        };
        if file.locked_by.as_deref().is_some_and(|owner| owner != requester) {
            return false;
        }
        file.locked_by = Some(requester.to_string());
        true
    }

    pub fn unlock_file(&mut self, requester: &str, hash: &str) -> bool {
        let Some(file) = self.files.iter_mut().find(|file| file.hash == hash) else {
            return false;
        };
        if file.locked_by.as_deref() != Some(requester) {
            return false;
        }
        file.locked_by = None;
        true
    }

    pub fn add_member(&mut self, member: RoomMember) -> bool {
        if self.has_member(&member.public_key) {
            return false;
        }
        self.members.push(member);
        true
    }

    pub fn apply_team_update(&mut self, requester: &str, update: &RoomUpdate, now: u64) -> Option<String> {
        let message = match update {
            RoomUpdate::AddAnnouncement { message, pinned } if self.can_edit(requester) => {
                let message = message.trim();
                if message.is_empty() {
                    return None;
                }
                self.announcements.push(RoomAnnouncement {
                    id: format!("announcement-{}", uuid_like_id()),
                    message: message.to_string(),
                    pinned: *pinned,
                    posted_by: requester.to_string(),
                    posted_at: now,
                });
                "posted an announcement".to_string()
            }
            RoomUpdate::SetAnnouncementPinned { id, pinned } if self.can_edit(requester) => {
                let announcement = self.announcements.iter_mut().find(|item| item.id == *id)?;
                announcement.pinned = *pinned;
                if *pinned { "pinned an announcement" } else { "unpinned an announcement" }.to_string()
            }
            RoomUpdate::AddClipboard { content } if self.can_upload(requester) => {
                let content = content.trim();
                if content.is_empty() || content.len() > 1024 * 1024 {
                    return None;
                }
                self.clipboard.push(ClipboardEntry {
                    id: format!("clipboard-{}", uuid_like_id()),
                    content: content.to_string(),
                    added_by: requester.to_string(),
                    added_at: now,
                });
                "added a shared clipboard entry".to_string()
            }
            RoomUpdate::RemoveClipboard { id } if self.can_edit(requester) => {
                let before = self.clipboard.len();
                self.clipboard.retain(|item| item.id != *id);
                (self.clipboard.len() != before).then(|| "removed a shared clipboard entry".to_string())?
            }
            RoomUpdate::LockFile { hash } if self.lock_file(requester, hash) => {
                "locked a file".to_string()
            }
            RoomUpdate::UnlockFile { hash } if self.unlock_file(requester, hash) => {
                "unlocked a file".to_string()
            }
            RoomUpdate::SetMemberRole { public_key, role }
                if self.can_delete(requester)
                    && *public_key != self.owner_public_key
                    && self.members.iter_mut().find(|member| member.public_key == *public_key).map(|member| member.role = *role).is_some() =>
            {
                "changed a member role".to_string()
            }
            RoomUpdate::AddNote { title, body }
                if self.can_edit(requester) && !title.trim().is_empty() && !body.trim().is_empty() =>
            {
                self.notes.push(RoomNote {
                    id: format!("note-{}", uuid_like_id()),
                    title: title.trim().to_string(),
                    body: body.trim().to_string(),
                    updated_by: requester.to_string(),
                    updated_at: now,
                });
                "added a shared note".to_string()
            }
            RoomUpdate::AddLink { title, url }
                if self.can_edit(requester)
                    && !title.trim().is_empty()
                    && (url.starts_with("https://") || url.starts_with("http://")) =>
            {
                self.links.push(RoomLink {
                    title: title.trim().to_string(),
                    url: url.trim().to_string(),
                    added_by: requester.to_string(),
                    added_at: now,
                });
                "added a shared link".to_string()
            }
            RoomUpdate::SetFileDownloads { hash, allowed } if self.can_edit(requester) => {
                let file = self.files.iter_mut().find(|file| file.hash == *hash)?;
                file.downloads_allowed = *allowed;
                if *allowed { "allowed file downloads" } else { "restricted file downloads" }.to_string()
            }
            RoomUpdate::SetSessionExpiration { expires_at } if self.can_delete(requester) => {
                self.session_expires_at = *expires_at;
                if expires_at.is_some() { "set a room expiration" } else { "removed room expiration" }.to_string()
            }
            _ => return None,
        };
        self.activity.push(ActivityEntry {
            message: message.clone(),
            actor: requester.to_string(),
            at: now,
        });
        if self.activity.len() > 200 {
            let excess = self.activity.len() - 200;
            self.activity.drain(..excess);
        }
        Some(message)
    }

    pub fn add_file(&mut self, file: RoomFile) -> bool {
        if self.files.iter().any(|existing| existing.hash == file.hash) {
            return false;
        }
        let mut file = file;
        file.version = self
            .files
            .iter()
            .filter(|existing| existing.name == file.name)
            .map(|existing| existing.version)
            .max()
            .unwrap_or(0)
            + 1;
        file.previous_hash = self
            .files
            .iter()
            .filter(|existing| existing.name == file.name)
            .max_by_key(|existing| existing.version)
            .map(|existing| existing.hash.clone());
        self.files.push(file);
        true
    }

    pub fn delete_file(&mut self, requester: &str, hash: &str) -> bool {
        if !self.can_delete(requester) {
            return false;
        }
        let before = self.files.len();
        self.files.retain(|file| file.hash != hash);
        self.files.len() != before
    }

    pub fn create_assignment(
        &mut self,
        requester: &str,
        title: String,
        deadline: u64,
        instructions: String,
        required_files: Vec<String>,
    ) -> Option<Assignment> {
        if !self.can_delete(requester) {
            return None;
        }
        let assignment = Assignment {
            id: format!("assignment-{}", uuid_like_id()),
            title,
            deadline,
            instructions,
            required_files,
            submissions: Vec::new(),
        };
        self.assignments.push(assignment.clone());
        Some(assignment)
    }

    pub fn record_submission(&mut self, submission: Submission) -> bool {
        let Some(assignment) = self
            .assignments
            .iter_mut()
            .find(|assignment| assignment.id == submission.assignment_id)
        else {
            return false;
        };
        assignment
            .submissions
            .retain(|existing| existing.student_public_key != submission.student_public_key);
        assignment.submissions.push(submission);
        true
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub device_name: String,
    /// Hex. Always the plain key in memory. On disk it is empty when `protected_key`
    /// holds the Windows DPAPI encrypted copy.
    #[serde(default)]
    pub private_key: String,
    /// Hex of the DPAPI encrypted private key (only readable by this Windows user).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected_key: Option<String>,
    pub public_key: String,
    #[serde(default)]
    pub peers: Vec<PairedPeer>,
    #[serde(default)]
    pub rooms: Vec<Room>,
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Shelf")
}

pub fn inbox_dir() -> PathBuf {
    data_dir().join("inbox")
}

pub fn room_dir(room_id: &str) -> PathBuf {
    data_dir().join("rooms").join(room_id)
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn create_room(&mut self, name: String) -> Room {
        let id = format!("room-{}", uuid_like_id());
        let room = Room {
            id,
            name,
            invite_code: format!("{:06}", room_code()),
            owner_public_key: self.public_key.clone(),
            members: vec![RoomMember {
                name: self.device_name.clone(),
                public_key: self.public_key.clone(),
                role: RoomRole::Owner,
            }],
            files: Vec::new(),
            assignments: Vec::new(),
            questions: Vec::new(),
            session_expires_at: None,
            notes: Vec::new(),
            links: Vec::new(),
            announcements: Vec::new(),
            clipboard: Vec::new(),
            activity: Vec::new(),
            pending_members: Vec::new(),
        };
        self.rooms.push(room.clone());
        room
    }

    pub fn load_or_create() -> Result<Config> {
        let path = config_path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            let loaded = serde_json::from_str::<Config>(&text)
                .ok()
                .and_then(|mut cfg| cfg.unlock().map(|migrate| (cfg, migrate)));
            match loaded {
                Some((cfg, migrate)) => {
                    if migrate {
                        // Re-save so a plain-text key from an older version gets encrypted.
                        let _ = cfg.save();
                    }
                    return Ok(cfg);
                }
                None => {
                    // Keep a copy of an unreadable file instead of silently destroying pairings.
                    let _ = std::fs::rename(&path, path.with_extension("json.bak"));
                }
            }
        }
        let params: snow::params::NoiseParams = crate::channel::NOISE_PARAMS.parse()?;
        let kp = snow::Builder::new(params).generate_keypair()?;
        let cfg = Config {
            device_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".to_string()),
            private_key: hex::encode(&kp.private),
            protected_key: None,
            public_key: hex::encode(&kp.public),
            peers: Vec::new(),
            rooms: Vec::new(),
        };
        cfg.save()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let path = config_path();
        let tmp = path.with_extension("json.tmp");
        // Encrypt the private key at rest when the OS can do it (Windows DPAPI).
        let mut disk = self.clone();
        if let Ok(raw) = hex::decode(&self.private_key) {
            if let Some(blob) = dpapi::protect(&raw) {
                disk.protected_key = Some(hex::encode(blob));
                disk.private_key = String::new();
            }
        }
        std::fs::write(&tmp, serde_json::to_vec_pretty(&disk)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Fills in `private_key` from the DPAPI copy. Returns None if the file holds no usable
    /// key, otherwise whether the file should be re-saved to encrypt a plain-text key.
    fn unlock(&mut self) -> Option<bool> {
        if let Some(blob_hex) = self.protected_key.take() {
            let blob = hex::decode(blob_hex).ok()?;
            let key = dpapi::unprotect(&blob)?;
            self.private_key = hex::encode(key);
            return Some(false);
        }
        if self.private_key.is_empty() {
            return None;
        }
        Some(cfg!(windows))
    }

    pub fn find_peer(&self, public_key: &[u8]) -> Option<&PairedPeer> {
        let k = hex::encode(public_key);
        self.peers.iter().find(|p| p.public_key == k)
    }
}

fn uuid_like_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{now:x}")
}

fn room_code() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    (now % 1_000_000) as u32
}

#[cfg(test)]
mod tests {
    use super::{Room, RoomFile, RoomMember, RoomRole, Submission};

    fn room() -> Room {
        Room {
            id: "room-1".into(),
            name: "Science Class".into(),
            invite_code: "123456".into(),
            owner_public_key: "teacher".into(),
            members: vec![RoomMember {
                name: "Student".into(),
                public_key: "student".into(),
                role: RoomRole::Member,
            }],
            files: vec![RoomFile {
                hash: "abc".into(),
                name: "worksheet.pdf".into(),
                size: 42,
                uploaded_by: "student".into(),
                uploaded_at: 1,
                submission: None,
                version: 1,
                previous_hash: None,
                downloads_allowed: true,
                locked_by: None,
            }],
            assignments: Vec::new(),
            questions: Vec::new(),
            session_expires_at: None,
            notes: Vec::new(),
            links: Vec::new(),
            announcements: Vec::new(),
            clipboard: Vec::new(),
            activity: Vec::new(),
            pending_members: Vec::new(),
        }
    }

    #[test]
    fn only_owner_can_delete_room_files() {
        let room = room();
        assert!(room.can_delete("teacher"));
        assert!(!room.can_delete("student"));
    }

    #[test]
    fn membership_is_checked_by_public_key() {
        let room = room();
        assert!(room.has_member("student"));
        assert!(!room.has_member("unknown"));
    }

    #[test]
    fn members_can_upload_but_only_owner_can_delete() {
        let mut room = room();
        assert!(room.add_member(RoomMember {
            name: "Second student".into(),
            public_key: "student-2".into(),
            role: RoomRole::Member,
        }));
        assert!(!room.add_member(RoomMember {
            name: "Duplicate".into(),
            public_key: "student".into(),
            role: RoomRole::Member,
        }));
        assert!(room.add_file(RoomFile {
            hash: "def".into(),
            name: "submission.txt".into(),
            size: 8,
            uploaded_by: "student".into(),
            uploaded_at: 2,
            submission: None,
            version: 0,
            previous_hash: None,
            downloads_allowed: true,
            locked_by: None,
        }));
        assert!(!room.delete_file("student", "def"));
        assert!(room.delete_file("teacher", "def"));
    }

    #[test]
    fn owner_creates_assignments_and_students_submit_once() {
        let mut room = room();
        let assignment = room
            .create_assignment(
                "teacher",
                "Lab report".into(),
                100,
                "Upload your report".into(),
                vec!["report.pdf".into()],
            )
            .unwrap();
        assert!(room.record_submission(Submission {
            assignment_id: assignment.id.clone(),
            file_hash: "abc".into(),
            student_public_key: "student".into(),
            submitted_at: 50,
        }));
        assert_eq!(room.assignments[0].submissions.len(), 1);
        assert!(room.record_submission(Submission {
            assignment_id: assignment.id,
            file_hash: "def".into(),
            student_public_key: "student".into(),
            submitted_at: 60,
        }));
        assert_eq!(room.assignments[0].submissions.len(), 1);
        assert_eq!(room.assignments[0].submissions[0].file_hash, "def");
        assert!(room
            .create_assignment("student", "Nope".into(), 100, String::new(), Vec::new())
            .is_none());
    }

    #[test]
    fn team_roles_versions_and_locks_are_enforced() {
        let mut room = room();
        room.add_member(RoomMember {
            name: "Teacher".into(),
            public_key: "teacher".into(),
            role: RoomRole::Owner,
        });
        room.members[0].role = RoomRole::Editor;
        assert!(room.lock_file("student", "abc"));
        assert!(!room.lock_file("teacher", "abc"));
        assert!(room.unlock_file("student", "abc"));
        assert!(room.add_file(RoomFile {
            hash: "new-hash".into(),
            name: "worksheet.pdf".into(),
            size: 50,
            uploaded_by: "student".into(),
            uploaded_at: 2,
            submission: None,
            version: 0,
            previous_hash: None,
            downloads_allowed: true,
            locked_by: None,
        }));
        let latest = room.files.iter().find(|file| file.hash == "new-hash").unwrap();
        assert_eq!(latest.version, 2);
        assert_eq!(latest.previous_hash.as_deref(), Some("abc"));
    }
}
