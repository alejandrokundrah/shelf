use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
}

impl Room {
    pub fn can_delete(&self, public_key: &str) -> bool {
        self.owner_public_key == public_key
    }

    pub fn has_member(&self, public_key: &str) -> bool {
        self.members.iter().any(|m| m.public_key == public_key)
    }

    pub fn add_member(&mut self, member: RoomMember) -> bool {
        if self.has_member(&member.public_key) {
            return false;
        }
        self.members.push(member);
        true
    }

    pub fn add_file(&mut self, file: RoomFile) -> bool {
        if self.files.iter().any(|existing| existing.hash == file.hash) {
            return false;
        }
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
    /// Hex. Stored in plain text in config.json (see README, security notes).
    pub private_key: String,
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
        };
        self.rooms.push(room.clone());
        room
    }

    pub fn load_or_create() -> Result<Config> {
        let path = config_path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<Config>(&text) {
                Ok(cfg) => return Ok(cfg),
                Err(_) => {
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
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
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
            }],
            assignments: Vec::new(),
            questions: Vec::new(),
            session_expires_at: None,
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
}
