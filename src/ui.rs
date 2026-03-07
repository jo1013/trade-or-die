use anyhow::Result;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph},
    DefaultTerminal,
};

pub fn draw_ui(terminal: &mut DefaultTerminal, stats: &str) -> Result<()> {
    terminal.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(f.area());

        let title =
            Paragraph::new(" Argo-Rust AI Agent ").block(Block::default().borders(Borders::ALL));
        f.render_widget(title, chunks[0]);

        let status =
            Paragraph::new(stats).block(Block::default().borders(Borders::ALL).title(" Status "));
        f.render_widget(status, chunks[1]);
    })?;
    Ok(())
}
