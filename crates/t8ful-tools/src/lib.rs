//! Built-in tool registry.
//!
//! Tools only implement behavior; plan-mode denials are decided centrally in
//! `t8ful-core` before a tool is dispatched.

pub mod agent_tools;
pub mod edit;
pub mod env;
pub mod patch;
pub mod paths;
pub mod plan;
pub mod process;
pub mod process_tools;
pub mod question;
pub mod read;
pub mod recall;
pub mod shell;
pub mod skill;
pub mod task_tools;
pub mod write;

mod fsutil;
mod text;

use std::sync::Arc;

use t8ful_core::tool::ToolRegistry;

use crate::process::ProcessManager;

pub use edit::EditFileTool;
pub use patch::ApplyPatchTool;
pub use plan::PresentPlanTool;
pub use process::{
    ForegroundOutcome, OutputBuffer, ProcessExit, ProcessManager as ManagedProcesses,
    ProcessSnapshot, ProcessState,
};
pub use process_tools::{
    ProcessListTool, ProcessOutputTool, ProcessStatusTool, ProcessStopTool, ProcessWaitTool,
};
pub use question::AskUserQuestionTool;
pub use read::ReadFileTool;
pub use recall::{NoRecall, RecallTool};
pub use shell::BashTool;
pub use skill::{NoSkills, SkillDocument, SkillLibrary, SkillSummary, SkillTool};
pub use write::WriteFileTool;

/// Create the built-in tool set backed by one process manager and the
/// discovered skill library.
pub fn builtin_registry(
    processes: Arc<ProcessManager>,
    skills: Arc<dyn SkillLibrary>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    registry.register(ReadFileTool);
    registry.register(WriteFileTool);
    registry.register(EditFileTool);
    registry.register(ApplyPatchTool);
    registry.register(AskUserQuestionTool);
    registry.register(PresentPlanTool);
    registry.register(SkillTool::new(skills));
    registry.register(BashTool::new(Arc::clone(&processes)));
    registry.register(ProcessOutputTool::new(Arc::clone(&processes)));
    registry.register(ProcessStatusTool::new(Arc::clone(&processes)));
    registry.register(ProcessListTool::new(Arc::clone(&processes)));
    registry.register(ProcessWaitTool::new(Arc::clone(&processes)));
    registry.register(ProcessStopTool::new(processes));

    registry
}

/// Names of the tools this crate provides, in registration order.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "apply_patch",
    "ask_user_question",
    "present_plan",
    "skill",
    "bash",
    "process_output",
    "process_status",
    "process_list",
    "process_wait",
    "process_stop",
];

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use t8ful_core::tool::{ToolContext, ToolEffect};

    use super::{BUILTIN_TOOL_NAMES, builtin_registry};

    #[test]
    fn registry_exposes_every_builtin_tool_once() {
        let processes = std::sync::Arc::new(super::ProcessManager::new(1024));
        let registry = builtin_registry(processes, std::sync::Arc::new(super::NoSkills));

        assert_eq!(registry.len(), BUILTIN_TOOL_NAMES.len());

        for name in BUILTIN_TOOL_NAMES {
            assert!(registry.get(name).is_some(), "missing tool {name}");
        }
    }

    #[test]
    fn read_side_tools_are_read_only() {
        let processes = std::sync::Arc::new(super::ProcessManager::new(1024));
        let registry = builtin_registry(processes, std::sync::Arc::new(super::NoSkills));
        let context = ToolContext::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"));

        let _ = context;

        for name in [
            "read_file",
            "skill",
            "process_output",
            "process_status",
            "process_list",
            "process_wait",
        ] {
            let tool = registry.get(name).expect("tool registered");

            assert_eq!(tool.effect(), ToolEffect::ReadOnly, "{name}");
        }

        for name in ["write_file", "edit_file", "apply_patch"] {
            let tool = registry.get(name).expect("tool registered");

            assert_eq!(tool.effect(), ToolEffect::Mutating, "{name}");
        }

        for name in ["ask_user_question", "present_plan", "process_stop"] {
            let tool = registry.get(name).expect("tool registered");

            assert_eq!(tool.effect(), ToolEffect::Control, "{name}");
        }

        let bash = registry.get("bash").expect("tool registered");

        assert_eq!(bash.effect(), ToolEffect::Process);
    }
}
