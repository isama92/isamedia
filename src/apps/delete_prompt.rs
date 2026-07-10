//! The "delete this movie/series" prompt, shared by the Sonarr and Radarr
//! tabs. Backend-agnostic: it carries the entity id and its title (for the
//! question) plus the two server options the user toggles before committing —
//! delete the files from disk, and add an import-list exclusion. Modal while
//! set; rendered with [`prompt::draw_toggles`].
//!
//! Unlike [`crate::apps::downloads::Downloads`], which owns its prompt in a
//! private field and `take`s it on commit, the owning `Browse` holds an
//! `Option<DeletePrompt>` and clears it itself, so this type never has to
//! close itself.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;

use crate::ui::prompt;

/// A pending whole-entity delete decision.
pub struct DeletePrompt {
    id: i64,
    title: String,
    delete_files: bool,
    add_exclusion: bool,
    /// Which toggle is focused: 0 = delete files, 1 = add exclusion.
    focus: usize,
}

/// What routing a key through the prompt resolved to.
pub enum DeleteAction {
    /// Key consumed; nothing for the app to do.
    Consumed,
    /// The user cancelled; the app should drop the prompt.
    Cancelled,
    /// The user committed; the app should issue the delete with these options
    /// and drop the prompt.
    Commit {
        id: i64,
        delete_files: bool,
        add_exclusion: bool,
    },
}

impl DeletePrompt {
    /// Open with the Radarr/Sonarr web defaults: delete files on, exclusion
    /// off, focus on the first toggle.
    pub fn new(id: i64, title: String) -> Self {
        Self {
            id,
            title,
            delete_files: true,
            add_exclusion: false,
            focus: 0,
        }
    }

    /// Route a key: up/k and down/j move focus (clamped to 0..=1); space
    /// toggles the focused option; enter commits; esc/q cancels. Same contract
    /// as [`crate::apps::downloads::Downloads::handle_prompt_key`].
    pub fn handle_key(&mut self, key: KeyEvent) -> DeleteAction {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.focus = self.focus.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.focus = (self.focus + 1).min(1),
            KeyCode::Char(' ') => {
                if self.focus == 0 {
                    self.delete_files = !self.delete_files;
                } else {
                    self.add_exclusion = !self.add_exclusion;
                }
            }
            KeyCode::Enter => {
                return DeleteAction::Commit {
                    id: self.id,
                    delete_files: self.delete_files,
                    add_exclusion: self.add_exclusion,
                };
            }
            KeyCode::Esc | KeyCode::Char('q') => return DeleteAction::Cancelled,
            _ => {}
        }
        DeleteAction::Consumed
    }

    /// Draw the prompt over the current frame.
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let title = format!("Delete {}?", self.title);
        let options = [
            ("Delete files", self.delete_files),
            ("Add import list exclusion", self.add_exclusion),
        ];
        prompt::draw_toggles(
            frame,
            area,
            &title,
            &options,
            self.focus,
            "space: toggle   enter: delete   esc: cancel",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn defaults_delete_files_on_exclusion_off() {
        let mut prompt = DeletePrompt::new(7, "The Matrix".into());
        match prompt.handle_key(key(KeyCode::Enter)) {
            DeleteAction::Commit {
                id,
                delete_files,
                add_exclusion,
            } => {
                assert_eq!(id, 7);
                assert!(delete_files);
                assert!(!add_exclusion);
            }
            _ => panic!("expected commit"),
        }
    }

    #[test]
    fn space_toggles_focused_option() {
        let mut prompt = DeletePrompt::new(1, "Show".into());
        // Focus starts on "delete files" (default on); space turns it off,
        // then down+space turns the exclusion on.
        prompt.handle_key(key(KeyCode::Char(' ')));
        prompt.handle_key(key(KeyCode::Down));
        prompt.handle_key(key(KeyCode::Char(' ')));
        match prompt.handle_key(key(KeyCode::Enter)) {
            DeleteAction::Commit {
                delete_files,
                add_exclusion,
                ..
            } => {
                assert!(!delete_files);
                assert!(add_exclusion);
            }
            _ => panic!("expected commit"),
        }
    }

    #[test]
    fn focus_is_clamped_to_the_two_rows() {
        let mut prompt = DeletePrompt::new(1, "Show".into());
        // Up past the top stays on row 0; many downs stop at row 1.
        prompt.handle_key(key(KeyCode::Up));
        assert_eq!(prompt.focus, 0);
        for _ in 0..5 {
            prompt.handle_key(key(KeyCode::Down));
        }
        assert_eq!(prompt.focus, 1);
    }

    #[test]
    fn esc_cancels() {
        let mut prompt = DeletePrompt::new(1, "Show".into());
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Esc)),
            DeleteAction::Cancelled
        ));
    }
}
