use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::domain::{Context, ContextMessage};
use crate::services::{Services, FsReadService, EnvironmentService};

/// Loads AGENTS.md files from multiple locations and adds their content as user messages to the context.
/// 
/// The search order is:
/// 1. `~/forge/AGENTS.md` - personal global guidance (if home directory available from environment)
/// 2. `AGENTS.md` at repo root - shared project notes  
/// 3. `AGENTS.md` in current working directory - sub-folder/feature specifics
pub async fn load_agents_md_context<S>(
    context: Context,
    current_working_directory: &Path,
    services: &S,
) -> anyhow::Result<Context>
where
    S: Services,
{
    let mut updated_context = context;
    
    // Define the AGENTS.md file locations in order of priority
    let agents_md_paths = get_agents_md_paths(current_working_directory, services);
    
    for (location, path) in agents_md_paths {
        // Use the FsReadService to read the file
        match services.read(path.to_string_lossy().to_string(), None, None).await {
            Ok(read_output) => {
                let content = match read_output.content {
                    crate::services::Content::File(content) => content,
                };
                
                debug!(
                    path = %path.display(),
                    location = %location,
                    "Loading AGENTS.md content"
                );
                
                // Add content as a user message with appropriate context
                let message_content = format!(
                    "<!-- AGENTS.md from {} -->\n{}",
                    location,
                    content.trim()
                );
                
                updated_context = updated_context.add_message(
                    ContextMessage::user(message_content, None)
                );
            }
            Err(_) => {
                debug!(
                    path = %path.display(),
                    location = %location,
                    "AGENTS.md file not found or not readable"
                );
            }
        }
    }
    
    Ok(updated_context)
}

/// Returns a list of potential AGENTS.md file paths in order of priority
fn get_agents_md_paths<S>(
    current_working_directory: &Path,
    services: &S,
) -> Vec<(String, PathBuf)>
where
    S: Services,
{
    let mut paths = Vec::new();
    
    // 1. Personal global guidance: ~/forge/AGENTS.md
    // Use the environment service to get the home directory if available
    let env = services.environment_service().get_environment();
    if let Some(home_path) = &env.home {
        let global_agents_md = home_path.join("forge").join("AGENTS.md");
        paths.push(("personal global guidance".to_string(), global_agents_md));
    } else {
        warn!("Could not determine home directory from environment, skipping global AGENTS.md");
    }
    
    // 2. Project-level shared notes: AGENTS.md at repo root
    let repo_root = find_repo_root(current_working_directory);
    if let Some(root) = repo_root {
        let project_agents_md = root.join("AGENTS.md");
        paths.push(("project root".to_string(), project_agents_md));
    } else {
        // If no repo root found, look in current directory's parent directories
        let mut parent = current_working_directory.to_path_buf();
        loop {
            let agents_md = parent.join("AGENTS.md");
            if agents_md.exists() {
                paths.push(("project level".to_string(), agents_md));
                break;
            }
            
            if let Some(parent_dir) = parent.parent() {
                parent = parent_dir.to_path_buf();
            } else {
                break;
            }
        }
    }
    
    // 3. Feature-specific guidance: AGENTS.md in current working directory
    let local_agents_md = current_working_directory.join("AGENTS.md");
    paths.push(("current directory".to_string(), local_agents_md));
    
    paths
}

/// Attempts to find the repository root by looking for common VCS indicators
fn find_repo_root(start_path: &Path) -> Option<PathBuf> {
    let mut current = start_path.to_path_buf();
    
    loop {
        // Check for common repository indicators
        let git_dir = current.join(".git");
        let hg_dir = current.join(".hg");
        let svn_dir = current.join(".svn");
        
        if git_dir.exists() || hg_dir.exists() || svn_dir.exists() {
            return Some(current);
        }
        
        // Move up to parent directory
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }
    
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_find_repo_root_with_git() {
        let temp_dir = TempDir::new().unwrap();
        let git_dir = temp_dir.path().join(".git");
        fs::create_dir(&git_dir).unwrap();
        
        let sub_dir = temp_dir.path().join("subdir");
        fs::create_dir(&sub_dir).unwrap();
        
        let result = find_repo_root(&sub_dir);
        assert_eq!(result, Some(temp_dir.path().to_path_buf()));
    }

    #[test]
    fn test_find_repo_root_no_vcs() {
        let temp_dir = TempDir::new().unwrap();
        let result = find_repo_root(temp_dir.path());
        assert_eq!(result, None);
    }
}