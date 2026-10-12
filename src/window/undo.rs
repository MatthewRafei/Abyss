//! Undo (Ctrl+Z): each file operation that can be taken back leaves a note
//! of exactly what it did, and undoing reverses just that. Deleting forever
//! can't be undone, and neither can replacing a file's content.

use super::*;

/// How many steps are kept.
const DEPTH: usize = 50;

/// How long the footer's Undo button stays after an operation.
const OFFER: std::time::Duration = std::time::Duration::from_secs(8);

/// What an operation changed, as much as is needed to put it back.
#[derive(Clone)]
pub(super) enum Change {
    /// Moved to the trash from these places.
    Trashed(Vec<gio::File>),
    /// Made by a copy, new folder, extract or compress: undoing trashes them.
    Created(Vec<gio::File>),
    /// Moved from the first place to the second.
    Moved(Vec<(gio::File, gio::File)>),
    /// Renamed: each file as it's named now, and its old name.
    Renamed(Vec<(gio::File, String)>),
}

#[derive(Clone)]
pub(super) struct Step {
    /// What it was, as the menus say it: "Move to Trash", "Rename".
    pub label: String,
    pub change: Change,
}

impl Win {
    /// Notes an operation that can be undone, and offers to in the footer.
    pub(super) fn record(&self, label: &str, change: Change) {
        let empty = match &change {
            Change::Trashed(v) | Change::Created(v) => v.is_empty(),
            Change::Moved(v) => v.is_empty(),
            Change::Renamed(v) => v.is_empty(),
        };
        if empty {
            return;
        }
        let mut steps = self.undo_steps.borrow_mut();
        steps.push(Step {
            label: label.to_owned(),
            change,
        });
        if steps.len() > DEPTH {
            steps.remove(0);
        }
        drop(steps);
        self.offer_undo(label);
    }

    /// The step Ctrl+Z would take back, if any.
    pub(super) fn last_step(&self) -> Option<String> {
        self.undo_steps.borrow().last().map(|s| s.label.clone())
    }

    /// Shows the footer's Undo button for a while.
    fn offer_undo(&self, label: &str) {
        self.undo_btn
            .set_tooltip_text(Some(&format!("Undo {label} (Ctrl+Z)")));
        self.undo_btn.set_visible(true);
        let seq = self.undo_seq.get().wrapping_add(1);
        self.undo_seq.set(seq);
        let button = self.undo_btn.downgrade();
        let latest = self.undo_seq.clone();
        glib::timeout_add_local_once(OFFER, move || {
            if let (Some(b), true) = (button.upgrade(), latest.get() == seq) {
                b.set_visible(false);
            }
        });
    }

    /// Takes back the last operation.
    pub(super) fn undo(self: &Rc<Self>) {
        if self.busy.get() {
            self.show_error("Wait for the running operation to finish, then undo");
            return;
        }
        let Some(step) = self.undo_steps.borrow_mut().pop() else {
            self.show_message("Nothing to undo");
            return;
        };
        self.undo_btn.set_visible(false);
        let label = step.label.clone();
        let w = Rc::downgrade(self);
        let doing = format!("Undoing {label}…");
        self.run_job(async move {
            let result = match step.change {
                Change::Trashed(origs) => ops::untrash(&origs).await,
                Change::Created(made) => match ops::trash(made.clone()).await {
                    // No trash there (a remote share): these were only
                    // copies or new items, so they can go for good.
                    Err(e) if e.matches(gio::IOErrorEnum::NotSupported) => ops::delete(made).await,
                    other => other,
                },
                Change::Moved(pairs) => ops::move_back(&pairs).await,
                Change::Renamed(renames) => ops::rename_many(renames).await.map(|_| ()),
            };
            if result.is_ok() {
                if let Some(win) = w.upgrade() {
                    win.show_message(&format!("Undone: {label}"));
                }
            }
            result
        });
        self.job_label.set_text(&doing);
    }
}
