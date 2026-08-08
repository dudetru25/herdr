use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MachineAddParams {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MachineImportParams {
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MachineImportOutcome {
    Added { alias: String },
    AlreadyExists { alias: String },
    Failed { alias: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MachineInfo {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SshHostInfo {
    pub alias: String,
    pub target: String,
    pub already_configured: bool,
}

impl From<&crate::config::MachineConfig> for MachineInfo {
    fn from(machine: &crate::config::MachineConfig) -> Self {
        Self {
            name: machine.name.clone(),
            target: machine.target.clone(),
            cwd: machine.cwd.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EmptyParams, Method, Request, ResponseResult};

    #[test]
    fn machine_add_request_and_response_round_trip() {
        let request = Request {
            id: "add-machine".to_string(),
            method: Method::MachineAdd(MachineAddParams {
                name: "build".to_string(),
                target: "builder@example.test".to_string(),
                cwd: Some("~/src/herdr".to_string()),
            }),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(
            serde_json::to_value(&request).unwrap()["method"],
            "machine.add"
        );

        let result = ResponseResult::MachineAdded {
            machine: MachineInfo {
                name: "build".to_string(),
                target: "builder@example.test".to_string(),
                cwd: Some("~/src/herdr".to_string()),
            },
        };
        let encoded = serde_json::to_string(&result).unwrap();
        let decoded: ResponseResult = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, result);
    }

    #[test]
    fn machine_ssh_hosts_request_and_response_round_trip() {
        let request = Request {
            id: "ssh-hosts".to_string(),
            method: Method::MachineSshHosts(EmptyParams::default()),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(
            serde_json::to_value(&request).unwrap()["method"],
            "machine.ssh_hosts"
        );

        let result = ResponseResult::MachineSshHosts {
            hosts: vec![SshHostInfo {
                alias: "build".to_string(),
                target: "builder@example.test:2222".to_string(),
                already_configured: true,
            }],
        };
        let encoded = serde_json::to_string(&result).unwrap();
        let decoded: ResponseResult = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, result);
    }

    #[test]
    fn machine_import_request_and_response_round_trip() {
        let request = Request {
            id: "import-machines".to_string(),
            method: Method::MachineImport(MachineImportParams {
                aliases: vec!["build".to_string(), "deploy".to_string()],
            }),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(
            serde_json::to_value(&request).unwrap()["method"],
            "machine.import"
        );

        let result = ResponseResult::MachineImported {
            outcomes: vec![
                MachineImportOutcome::Added {
                    alias: "build".to_string(),
                },
                MachineImportOutcome::AlreadyExists {
                    alias: "deploy".to_string(),
                },
                MachineImportOutcome::Failed {
                    alias: "missing".to_string(),
                    reason: "SSH host alias was not discovered".to_string(),
                },
            ],
        };
        let encoded = serde_json::to_string(&result).unwrap();
        let decoded: ResponseResult = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, result);
    }
}
