use crate::authorship::transcript::{AiTranscript, Message};
use crate::authorship::working_log::{AgentId, CheckpointKind};
use crate::commands::checkpoint_agent::agent_presets::{AgentCheckpointFlags, AgentCheckpointPreset, AgentRunResult};
use crate::error::GitAiError;
use serde_json::Value;
use std::collections::HashMap;

/// Trae IDE to checkpoint preset
pub struct TraePreset;

impl AgentCheckpointPreset for TraePreset {
    fn run(&self, flags: AgentCheckpointFlags) -> Result<AgentRunResult, GitAiError> {
        let hook_input_json = flags.hook_input.ok_or_else(|| {
            GitAiError::PresetError("hook_input is required for Trae preset".to_string())
        })?;

        // Debug: Print received hook input
        eprintln!("[TraePreset] Received hook input: {}", hook_input_json);

        let hook_data: Value = serde_json::from_str(&hook_input_json)
            .map_err(|e| GitAiError::PresetError(format!("Invalid JSON in hook_input: {}", e)))?;

        let hook_event_name = hook_data
            .get("hook_event_name")
            .or_else(|| hook_data.get("hookEventName"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                GitAiError::PresetError("hook_event_name not found in hook_input".to_string())
            })?
            .to_string();

        // Extract conversation_id/session_id from hook input
        let conversation_id = hook_data
            .get("conversation_id")
            .or_else(|| hook_data.get("session_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                GitAiError::PresetError("conversation_id or session_id not found in hook_input".to_string())
            })?
            .to_string();

        // Extract model from hook input
        let model = hook_data
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Extract repo working directory
        let repo_working_dir = hook_data
            .get("cwd")
            .or_else(|| hook_data.get("repo_working_dir"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Validate hook_event_name
        if hook_event_name != "preToolUse" && hook_event_name != "postToolUse" {
            return Err(GitAiError::PresetError(format!(
                "Invalid hook_event_name: {}. Expected 'preToolUse' or 'postToolUse'",
                hook_event_name
            )));
        }

        // Extract file_path from tool_input if present
        let file_path = hook_data
            .get("tool_input")
            .and_then(|ti| ti.get("file_path"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let agent_id = AgentId {
            tool: "trae".to_string(),
            id: conversation_id.clone(),
            model: model.clone(),
        };

        if hook_event_name == "preToolUse" {
            let will_edit = if !file_path.is_empty() {
                Some(vec![file_path.clone()])
            } else {
                None
            };

            // early return, we're just adding a human checkpoint.
            return Ok(AgentRunResult {
                agent_id,
                agent_metadata: None,
                checkpoint_kind: CheckpointKind::Human,
                transcript: None,
                repo_working_dir,
                edited_filepaths: None,
                will_edit_filepaths: will_edit,
                dirty_files: None,
                captured_checkpoint_id: None,
            });
        }

        // Read transcript from JSONL file if available
        let transcript_path = hook_data
            .get("transcript_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let transcript = if let Some(ref tp) = transcript_path {
            match Self::transcript_and_model_from_trae_jsonl(tp) {
                Ok((transcript, _)) => transcript,
                Err(e) => {
                    eprintln!(
                        "[Warning] Failed to parse Trae JSONL at {}: {}. Will retry at commit.",
                        tp, e
                    );
                    AiTranscript::new()
                }
            }
        } else {
            eprintln!("[Warning] No transcript_path in Trae hook input. Will retry at commit.");
            AiTranscript::new()
        };

        let edited_filepaths = if !file_path.is_empty() {
            Some(vec![file_path.to_string()])
        } else {
            None
        };

        // Store transcript_path in metadata for re-reading at commit time
        let agent_metadata =
            transcript_path.map(|tp| HashMap::from([("transcript_path".to_string(), tp)]));

        Ok(AgentRunResult {
            agent_id,
            agent_metadata,
            checkpoint_kind: CheckpointKind::AiAgent,
            transcript: Some(transcript),
            repo_working_dir,
            edited_filepaths,
            will_edit_filepaths: None,
            dirty_files: None,
            captured_checkpoint_id: None,
        })
    }
}

impl TraePreset {
    /// Parse a Trae JSONL transcript file into a transcript.
    /// Each line is a JSON object with a "type" field.
    pub fn transcript_and_model_from_trae_jsonl(
        transcript_path: &str,
    ) -> Result<(AiTranscript, Option<String>), GitAiError> {
        let content = std::fs::read_to_string(transcript_path).map_err(GitAiError::IoError)?;

        let mut transcript = AiTranscript::new();
        let mut model = None;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let entry: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue, // skip malformed lines
            };

            let entry_type = match entry.get("type").and_then(|v| v.as_str()) {
                Some(t) => t,
                None => continue,
            };

            let timestamp = entry
                .get("timestamp")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            match entry_type {
                "user" => {
                    if let Some(content) = entry.get("content").and_then(|v| v.as_str()) {
                        let trimmed = content.trim();
                        if !trimmed.is_empty() {
                            transcript.add_message(Message::User {
                                text: trimmed.to_string(),
                                timestamp: timestamp.clone(),
                            });
                        }
                    }
                }
                "assistant" => {
                    // Extract model from assistant messages if we haven't found it yet
                    if model.is_none()
                        && let Some(model_str) = entry.get("model").and_then(|v| v.as_str())
                    {
                        model = Some(model_str.to_string());
                    }

                    if let Some(content) = entry.get("content").and_then(|v| v.as_str()) {
                        let trimmed = content.trim();
                        if !trimmed.is_empty() {
                            transcript.add_message(Message::Assistant {
                                text: trimmed.to_string(),
                                timestamp: timestamp.clone(),
                            });
                        }
                    }

                    // Handle tool calls
                    if let Some(tool_calls) = entry.get("tool_calls").and_then(|v| v.as_array()) {
                        for tool_call in tool_calls {
                            if let Some(name) = tool_call.get("name").and_then(|v| v.as_str()) {
                                let args = tool_call.get("args").cloned().unwrap_or_else(|| {
                                    Value::Object(serde_json::Map::new())
                                });

                                transcript.add_message(Message::ToolUse {
                                    name: name.to_string(),
                                    input: args,
                                    timestamp: timestamp.clone(),
                                });
                            }
                        }
                    }
                }
                _ => continue, // Skip unknown message types
            }
        }

        Ok((transcript, model))
    }
}