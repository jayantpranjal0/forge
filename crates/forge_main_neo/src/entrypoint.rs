use crate::run;

pub async fn main_neo(experimental_no_stdout_tool: bool) -> anyhow::Result<()> {
    color_eyre::install().unwrap();
    let terminal = ratatui::init();
    let result = run(terminal, experimental_no_stdout_tool).await;
    ratatui::restore();
    result
}
