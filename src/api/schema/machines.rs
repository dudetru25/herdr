use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MachineAddParams {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MachineInfo {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
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
    use crate::api::schema::{Method, Request, ResponseResult};

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
}
