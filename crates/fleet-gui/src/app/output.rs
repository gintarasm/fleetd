//! Save Output…, Open Output in Editor and Copy Last Output (ADR 0086).
//! The output is fleetd's whole history of the agent plus its screen, as
//! `fleet history --plain` prints it, read over a connection of its own
//! and written to disk off the UI thread. File names and messages are
//! [`crate::core::output`]'s.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use gpui::{App, AsyncApp, ClipboardItem, Entity, Global, PathPromptOptions};

use super::app_state::{AppState, OutputJob};
use crate::core::ended::last_output;
use crate::core::output::{
    default_save_dir, failed_text, file_name, not_removed_text, saved_many_text, saved_text, today,
    unique_path, write_output, write_temp,
};
use crate::core::session::{PaneId, lock};
use crate::core::sidebar::display_name;

/// Opens a file with its default app.
pub type OpenFile = dyn Fn(&Path) -> io::Result<()> + Send + Sync;

/// [`OpenFile`] as a global, so tests can stand in for `open`; without
/// it, `open` runs.
#[derive(Clone)]
pub struct Opener(pub Arc<OpenFile>);

impl Global for Opener {}

/// The folder the last save went to: the save panel opens there next.
struct LastSaveDir(PathBuf);

impl Global for LastSaveDir {}

/// `open <path>` (macOS: the file's default app).
fn open_with_default_app(path: &Path) -> io::Result<()> {
    let status = Command::new("open").arg(path).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("open exited with {status}")))
    }
}

/// The agent's display name and suggested file name, if it is listed.
fn names(app: &AppState, agent_id: u64) -> Option<(String, String)> {
    let row = app.agents.iter().find(|r| r.info.id == agent_id)?;
    Some((display_name(&row.info), file_name(&row.info, &today())))
}

/// Shows the outcome of an output job: `Ok` as a notice, `Err` as an error.
fn report(app: &Entity<AppState>, outcome: Result<String, String>, cx: &mut AsyncApp) {
    let _ = app.update(cx, |a, cx| match outcome {
        Ok(notice) => a.show_notice(notice, cx),
        Err(error) => a.show_error(error, cx),
    });
}

/// Reads the agent's output with `job` and writes it to `path`.
fn read_and_write(job: OutputJob, path: &Path) -> Result<(), String> {
    let text = job()?;
    write_output(path, &text).map_err(|e| e.to_string())
}

/// Save Output…: asks where (a native save panel, `<name>-<date>.txt`
/// in the folder of the last save or ~/Downloads), then reads the
/// agent's output and writes it there, off the UI thread.
pub fn save_output(app: Entity<AppState>, agent_id: u64, cx: &mut App) {
    let a = app.read(cx);
    let Some((name, file)) = names(a, agent_id) else {
        return;
    };
    let dir = cx
        .try_global::<LastSaveDir>()
        .map_or_else(|| default_save_dir(&a.home), |d| d.0.clone());
    let home = a.home.clone();
    let chosen = cx.prompt_for_new_path(&dir, Some(&file));
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(path))) = chosen.await else {
            return;
        };
        if let Some(parent) = path.parent() {
            let parent = parent.to_owned();
            let _ = cx.update(|cx| cx.set_global(LastSaveDir(parent)));
        }
        let Ok(job) = app.read_with(cx, |a, _| a.read_output(agent_id)) else {
            return;
        };
        let written = path.clone();
        let outcome = cx
            .background_executor()
            .spawn(async move { read_and_write(job, &written) })
            .await;
        let outcome = outcome
            .map(|()| saved_text(&path, &home))
            .map_err(|e| failed_text("save", &name, &e));
        report(&app, outcome, cx);
    })
    .detach();
}

/// Open Output in Editor: writes the agent's output to a file under the
/// temp dir and opens it with the default app, off the UI thread.
pub fn open_output(app: Entity<AppState>, agent_id: u64, cx: &mut App) {
    let a = app.read(cx);
    let Some((name, file)) = names(a, agent_id) else {
        return;
    };
    let job = a.read_output(agent_id);
    let opener = cx
        .try_global::<Opener>()
        .cloned()
        .unwrap_or_else(|| Opener(Arc::new(open_with_default_app)));
    cx.spawn(async move |cx| {
        let outcome = cx
            .background_executor()
            .spawn(async move {
                let text = job()?;
                let path =
                    write_temp(&std::env::temp_dir(), &file, &text).map_err(|e| e.to_string())?;
                (opener.0)(&path).map_err(|e| e.to_string())
            })
            .await;
        if let Err(e) = outcome {
            report(&app, Err(failed_text("open", &name, &e)), cx);
        }
    })
    .detach();
}

/// Remove Exited's "Save Output First…": asks for a folder, then
/// [`save_all_then_remove`].
pub fn save_ended_then_remove(app: Entity<AppState>, ids: Vec<u64>, cx: &mut App) {
    if ids.is_empty() {
        return;
    }
    let chosen = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Save Here".into()),
    });
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = chosen.await else {
            return;
        };
        let Some(dir) = paths.into_iter().next() else {
            return;
        };
        let _ = cx.update(|cx| save_all_then_remove(app, ids, dir, cx));
    })
    .detach();
}

/// Saves each agent's output into `dir` (`<name>-<date>.txt`, numbered
/// when taken), off the UI thread, then removes the agents whose output
/// was saved. One that could not be saved stays, and the error says why.
pub fn save_all_then_remove(app: Entity<AppState>, ids: Vec<u64>, dir: PathBuf, cx: &mut App) {
    let a = app.read(cx);
    let home = a.home.clone();
    let jobs: Vec<(u64, String, String, OutputJob)> = ids
        .into_iter()
        .filter_map(|id| {
            let (name, file) = names(a, id)?;
            Some((id, name, file, a.read_output(id)))
        })
        .collect();
    cx.set_global(LastSaveDir(dir.clone()));
    cx.spawn(async move |cx| {
        let target = dir.clone();
        let (saved, errors) = cx
            .background_executor()
            .spawn(async move {
                let mut saved = Vec::new();
                let mut errors = Vec::new();
                for (id, name, file, job) in jobs {
                    let path = unique_path(&target, &file, Path::exists);
                    match read_and_write(job, &path) {
                        Ok(()) => saved.push(id),
                        Err(e) => errors.push(failed_text("save", &name, &e)),
                    }
                }
                (saved, errors)
            })
            .await;
        let count = saved.len();
        let _ = app.update(cx, |a, cx| {
            a.forget_all(saved);
            match not_removed_text(&errors) {
                Some(e) => a.show_error(e, cx),
                None => a.show_notice(saved_many_text(count, &dir, &home), cx),
            }
        });
    })
    .detach();
}

/// Copy Last Output: the pane's screen (its grid, not the scrolled view)
/// as text, blank rows around it dropped. Nothing for a blank screen.
pub fn copy_last_output(app: &Entity<AppState>, pane: PaneId, cx: &mut App) {
    let Some(view) = app.read(cx).pane_view(pane).cloned() else {
        return;
    };
    let rows = lock(view.read(cx).model_handle()).terminal().visible_text();
    if let Some(text) = last_output(&rows) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}
