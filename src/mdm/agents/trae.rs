use crate::error::GitAiError;
use crate::mdm::hook_installer::{
    HookCheckResult, HookInstaller, HookInstallerParams, InstallResult, UninstallResult,
};
use crate::mdm::utils::{
    generate_diff, home_dir, install_vsc_editor_extension, is_github_codespaces,
    is_vsc_editor_extension_installed, resolve_editor_cli, settings_paths_for_products,
    should_process_settings_target, write_atomic,
};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

// Command patterns for hooks
const TRAE_PRE_TOOL_USE_CMD: &str = "checkpoint trae --hook-input stdin";
const TRAE_POST_TOOL_USE_CMD: &str = "checkpoint trae --hook-input stdin";

/// Trae product variant (international or China version)
#[derive(Clone, Copy, Debug, PartialEq)]
enum TraeVariant {
    International,
    CN,
}

pub struct TraeInstaller;

impl TraeInstaller {
    fn hooks_path(variant: TraeVariant) -> PathBuf {
        let dir = match variant {
            TraeVariant::International => ".trae",
            TraeVariant::CN => ".trae-cn",
        };
        home_dir().join(dir).join("hooks.json")
    }

    fn settings_targets() -> Vec<PathBuf> {
        settings_paths_for_products(&["Trae", "Trae-CN"])
    }

    fn detect_variants() -> Vec<TraeVariant> {
        let mut variants = Vec::new();
        if home_dir().join(".trae").exists() {
            variants.push(TraeVariant::International);
        }
        if home_dir().join(".trae-cn").exists()
            || home_dir().join(".trae-cn-server").exists()
            || home_dir().join(".trae-aicc").exists()
        {
            variants.push(TraeVariant::CN);
        }
        // If neither is explicitly detected but CLI is available,
        // default to international to preserve existing behavior
        if variants.is_empty() && resolve_editor_cli("trae").is_some() {
            variants.push(TraeVariant::International);
        }
        variants
    }

    fn is_trae_checkpoint_command(cmd: &str) -> bool {
        cmd.contains("git-ai checkpoint trae")
            || (cmd.contains("git-ai") && cmd.contains("checkpoint") && cmd.contains("trae"))
    }

    fn check_hooks_for_variant(
        &self,
        _params: &HookInstallerParams,
        variant: TraeVariant,
    ) -> Result<HookCheckResult, GitAiError> {
        let hooks_path = Self::hooks_path(variant);

        if !hooks_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let content = fs::read_to_string(&hooks_path)?;
        let existing: Value = serde_json::from_str(&content).unwrap_or_else(|_| json!({}));

        let has_hooks = existing
            .get("hooks")
            .and_then(|h| h.get("preToolUse"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(Self::is_trae_checkpoint_command)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: has_hooks,
            hooks_up_to_date: has_hooks,
        })
    }

    fn install_hooks_for_variant(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
        variant: TraeVariant,
    ) -> Result<Option<String>, GitAiError> {
        let hooks_path = Self::hooks_path(variant);

        // Ensure directory exists
        if let Some(dir) = hooks_path.parent() {
            fs::create_dir_all(dir)?;
        }

        // Read existing content as string
        let existing_content = if hooks_path.exists() {
            fs::read_to_string(&hooks_path)?
        } else {
            String::new()
        };

        // Parse existing JSON if present, else start with empty object
        let existing: Value = if existing_content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&existing_content)?
        };

        // Build commands with absolute path
        let pre_tool_use_cmd = format!(
            "{} {}",
            params.binary_path.display(),
            TRAE_PRE_TOOL_USE_CMD
        );
        let post_tool_use_cmd = format!(
            "{} {}",
            params.binary_path.display(),
            TRAE_POST_TOOL_USE_CMD
        );

        // Desired hooks payload for Trae
        let desired: Value = json!({
            "version": 1,
            "hooks": {
                "preToolUse": [
                    {
                        "command": pre_tool_use_cmd
                    }
                ],
                "postToolUse": [
                    {
                        "command": post_tool_use_cmd
                    }
                ]
            }
        });

        // Merge desired into existing
        let mut merged = existing.clone();

        // Ensure version is set
        if merged.get("version").is_none()
            && let Some(obj) = merged.as_object_mut()
        {
            obj.insert("version".to_string(), json!(1));
        }

        // Merge hooks object
        let mut hooks_obj = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));

        // Process both hook types
        for hook_name in &["preToolUse", "postToolUse"] {
            let desired_hooks = desired
                .get("hooks")
                .and_then(|h| h.get(*hook_name))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Get existing hooks array for this hook type
            let mut existing_hooks = hooks_obj
                .get(*hook_name)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Update outdated git-ai checkpoint commands (or add if missing)
            for desired_hook in desired_hooks {
                let desired_cmd = desired_hook.get("command").and_then(|c| c.as_str());
                if desired_cmd.is_none() {
                    continue;
                }
                let desired_cmd = desired_cmd.unwrap();

                // Look for existing git-ai checkpoint trae commands
                let mut found_idx = None;
                let mut needs_update = false;

                for (idx, existing_hook) in existing_hooks.iter().enumerate() {
                    if let Some(existing_cmd) =
                        existing_hook.get("command").and_then(|c| c.as_str())
                        && Self::is_trae_checkpoint_command(existing_cmd)
                    {
                        found_idx = Some(idx);
                        if existing_cmd != desired_cmd {
                            needs_update = true;
                        }
                        break;
                    }
                }

                match found_idx {
                    Some(idx) if needs_update => {
                        existing_hooks[idx] = desired_hook.clone();
                    }
                    Some(_) => {
                        // Already up to date, skip
                    }
                    None => {
                        // No existing command, add new one
                        existing_hooks.push(desired_hook.clone());
                    }
                }
            }

            // Write back merged hooks for this hook type
            if let Some(obj) = hooks_obj.as_object_mut() {
                obj.insert(hook_name.to_string(), Value::Array(existing_hooks));
            }
        }

        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        // Check if there are semantic changes (compare JSON values, not strings)
        if existing == merged {
            return Ok(None);
        }

        // Generate new content
        let new_content = serde_json::to_string_pretty(&merged)?;

        // Generate diff
        let diff_output = generate_diff(&hooks_path, &existing_content, &new_content);

        // Write if not dry-run
        if !dry_run {
            write_atomic(&hooks_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }

    fn uninstall_hooks_for_variant(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
        variant: TraeVariant,
    ) -> Result<Option<String>, GitAiError> {
        let hooks_path = Self::hooks_path(variant);

        if !hooks_path.exists() {
            return Ok(None);
        }

        let existing_content = fs::read_to_string(&hooks_path)?;
        let existing: Value = serde_json::from_str(&existing_content)?;

        let mut merged = existing.clone();
        let mut hooks_obj = match merged.get("hooks").cloned() {
            Some(h) => h,
            None => return Ok(None),
        };

        let mut changed = false;

        // Remove git-ai checkpoint trae commands from both hook types
        for hook_name in &["preToolUse", "postToolUse"] {
            if let Some(hooks_array) = hooks_obj.get_mut(*hook_name).and_then(|v| v.as_array_mut())
            {
                let original_len = hooks_array.len();
                hooks_array.retain(|hook| {
                    if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                        !Self::is_trae_checkpoint_command(cmd)
                    } else {
                        true
                    }
                });
                if hooks_array.len() != original_len {
                    changed = true;
                }
            }
        }

        if !changed {
            return Ok(None);
        }

        // Write back hooks to merged
        if let Some(root) = merged.as_object_mut() {
            root.insert("hooks".to_string(), hooks_obj);
        }

        let new_content = serde_json::to_string_pretty(&merged)?;
        let diff_output = generate_diff(&hooks_path, &existing_content, &new_content);

        if !dry_run {
            write_atomic(&hooks_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }
}

impl HookInstaller for TraeInstaller {
    fn name(&self) -> &str {
        "Trae"
    }

    fn id(&self) -> &str {
        "trae"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["Trae", "trae", "trae-cn", "trae-cn-server"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_cli = resolve_editor_cli("trae").is_some();
        let has_dotfiles = home_dir().join(".trae").exists();
        let has_settings_targets = Self::settings_targets()
            .iter()
            .any(|path| should_process_settings_target(path));

        let variants = Self::detect_variants();
        let has_any_variant = !variants.is_empty() || has_cli || has_dotfiles;

        if !has_any_variant && !has_settings_targets {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        // Aggregate hook status across all detected variants
        let mut any_installed = false;
        let mut any_missing = false;

        for variant in &variants {
            let result = self.check_hooks_for_variant(params, *variant)?;
            if result.hooks_installed {
                any_installed = true;
            } else {
                any_missing = true;
            }
        }

        // If no specific variant detected but tool signs exist, check international default
        if variants.is_empty() {
            let result = self.check_hooks_for_variant(params, TraeVariant::International)?;
            any_installed = result.hooks_installed;
            any_missing = !result.hooks_installed;
        }

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: any_installed && !any_missing,
            hooks_up_to_date: any_installed && !any_missing,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let variants = Self::detect_variants();
        let variants_to_process = if variants.is_empty() {
            vec![TraeVariant::International]
        } else {
            variants
        };

        let mut combined_diff = String::new();
        let mut any_changed = false;

        for variant in &variants_to_process {
            if let Some(diff) = self.install_hooks_for_variant(params, dry_run, *variant)? {
                if !combined_diff.is_empty() {
                    combined_diff.push('\n');
                }
                combined_diff.push_str(&diff);
                any_changed = true;
            }
        }

        if any_changed {
            Ok(Some(combined_diff))
        } else {
            Ok(None)
        }
    }

    fn uninstall_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let variants = Self::detect_variants();
        let variants_to_process = if variants.is_empty() {
            vec![TraeVariant::International]
        } else {
            variants
        };

        let mut combined_diff = String::new();
        let mut any_changed = false;

        for variant in &variants_to_process {
            if let Some(diff) = self.uninstall_hooks_for_variant(params, dry_run, *variant)? {
                if !combined_diff.is_empty() {
                    combined_diff.push('\n');
                }
                combined_diff.push_str(&diff);
                any_changed = true;
            }
        }

        if any_changed {
            Ok(Some(combined_diff))
        } else {
            Ok(None)
        }
    }

    fn install_extras(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Vec<InstallResult>, GitAiError> {
        let mut results = Vec::new();

        // Skip extension installation in GitHub Codespaces
        if is_github_codespaces() {
            results.push(InstallResult {
                changed: false,
                diff: None,
                message: "Trae: Unable to install extension in GitHub Codespaces. Add to your devcontainer.json: \"customizations\": { \"vscode\": { \"extensions\": [\"git-ai.git-ai-vscode\"] } }".to_string(),
            });
            return Ok(results);
        }

        // Install VS Code extension
        if let Some(cli) = resolve_editor_cli("trae") {
            match is_vsc_editor_extension_installed(&cli, "git-ai.git-ai-vscode") {
                Ok(true) => {
                    results.push(InstallResult {
                        changed: false,
                        diff: None,
                        message: "Trae: Extension already installed".to_string(),
                    });
                }
                Ok(false) => {
                    if dry_run {
                        results.push(InstallResult {
                            changed: true,
                            diff: None,
                            message: "Trae: Pending extension install".to_string(),
                        });
                    } else {
                        println!("Installing extensions...");
                        println!("\tInstalling extension 'git-ai.git-ai-vscode'...");
                        match install_vsc_editor_extension(&cli, "git-ai.git-ai-vscode") {
                            Ok(()) => {
                                results.push(InstallResult {
                                    changed: true,
                                    diff: None,
                                    message: "\tExtension 'git-ai.git-ai-vscode' was successfully installed.".to_string(),
                                });
                            }
                            Err(e) => {
                                tracing::debug!(
                                    "Trae: Error automatically installing extension: {}",
                                    e
                                );
                                results.push(InstallResult {
                                    changed: false,
                                    diff: None,
                                    message: "Trae: Unable to automatically install extension. Please cmd+click on the following link to install: trae:extension/git-ai.git-ai-vscode (or search for 'git-ai-vscode' in the Trae extensions tab)".to_string(),
                                });
                            }
                        }
                    }
                }
                Err(e) => {
                    results.push(InstallResult {
                        changed: false,
                        diff: None,
                        message: format!("Trae: Failed to check extension: {}", e),
                    });
                }
            }
        } else {
            results.push(InstallResult {
                changed: false,
                diff: None,
                message: "Trae: Unable to automatically install extension. Please cmd+click on the following link to install: trae:extension/git-ai.git-ai-vscode (or search for 'git-ai-vscode' in the Trae extensions tab)".to_string(),
            });
        }

        // Configure git.path
        {
            use crate::mdm::utils::{git_shim_path_string, update_git_path_setting};

            let git_path = git_shim_path_string();
            for settings_path in Self::settings_targets() {
                if !should_process_settings_target(&settings_path) {
                    continue;
                }

                match update_git_path_setting(&settings_path, &git_path, dry_run) {
                    Ok(Some(diff)) => {
                        results.push(InstallResult {
                            changed: true,
                            diff: Some(diff),
                            message: format!(
                                "Trae: git.path updated in {}",
                                settings_path.display()
                            ),
                        });
                    }
                    Ok(None) => {
                        results.push(InstallResult {
                            changed: false,
                            diff: None,
                            message: format!(
                                "Trae: git.path already configured in {}",
                                settings_path.display()
                            ),
                        });
                    }
                    Err(e) => {
                        results.push(InstallResult {
                            changed: false,
                            diff: None,
                            message: format!("Trae: Failed to configure git.path: {}", e),
                        });
                    }
                }
            }
        }

        Ok(results)
    }

    fn uninstall_extras(
        &self,
        _params: &HookInstallerParams,
        _dry_run: bool,
    ) -> Result<Vec<UninstallResult>, GitAiError> {
        Ok(vec![UninstallResult {
            changed: false,
            diff: None,
            message: "Trae: Extension must be uninstalled manually through the editor".to_string(),
        }])
    }
}
