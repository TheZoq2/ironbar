use super::{Visibility, Workspace, WorkspaceClient, WorkspaceId, WorkspaceUpdate};
use crate::{arc_mut, lock, send};
use color_eyre::Result;
use hyprland::data::{Workspace as HWorkspace, Workspaces};
use hyprland::dispatch::{Dispatch, DispatchType, WorkspaceIdentifierWithSpecial};
use hyprland::event_listener::EventListener;
use hyprland::prelude::*;
use hyprland::shared::WorkspaceType;
use lazy_static::lazy_static;
use tokio::sync::broadcast::{channel, Receiver, Sender};
use tokio::task::spawn_blocking;
use tracing::{debug, error, info};

pub struct EventClient {
    workspace_tx: Sender<WorkspaceUpdate>,
    _workspace_rx: Receiver<WorkspaceUpdate>,
}

impl EventClient {
    fn new() -> Self {
        let (workspace_tx, workspace_rx) = channel(16);

        Self {
            workspace_tx,
            _workspace_rx: workspace_rx,
        }
    }

    fn listen_workspace_events(&self) {
        info!("Starting Hyprland event listener");

        let tx = self.workspace_tx.clone();

        spawn_blocking(move || {
            let mut event_listener = EventListener::new();

            // we need a lock to ensure events don't run at the same time
            let lock = arc_mut!(());

            // cache the active workspace since Hyprland doesn't give us the prev active
            let active = Self::get_active_workspace().expect("Failed to get active workspace");
            let active = arc_mut!(Some(active));

            {
                let tx = tx.clone();
                let lock = lock.clone();
                let active = active.clone();

                event_listener.add_workspace_added_handler(move |workspace_type| {
                    let _lock = lock!(lock);
                    debug!("Added workspace: {workspace_type:?}");

                    let workspace_name = get_workspace_id(workspace_type);
                    let prev_workspace = lock!(active);

                    let workspace = Self::get_workspace(&workspace_name, prev_workspace.as_ref());

                    if let Some(workspace) = workspace {
                        send!(tx, WorkspaceUpdate::Add(workspace));
                    }
                });
            }

            {
                let tx = tx.clone();
                let lock = lock.clone();
                let active = active.clone();

                event_listener.add_workspace_change_handler(move |workspace_type| {
                    let _lock = lock!(lock);

                    let mut prev_workspace = lock!(active);

                    debug!(
                        "Received workspace change: {:?} -> {workspace_type:?}",
                        prev_workspace.as_ref().map(|w| &w.id)
                    );

                    let workspace_name = get_workspace_id(workspace_type);
                    let workspace = Self::get_workspace(&workspace_name, prev_workspace.as_ref());

                    workspace.map_or_else(
                        || {
                            error!("Unable to locate workspace {workspace_name:?}");
                        },
                        |workspace| {
                            // there may be another type of update so dispatch that regardless of focus change
                            send!(tx, WorkspaceUpdate::Update(workspace.clone()));
                            if !workspace.visibility.is_focused() {
                                Self::send_focus_change(&mut prev_workspace, workspace, &tx);
                            }
                        },
                    );
                });
            }

            macro_rules! workspace_batch_event {
                ($event:ident) => {
                    let tx = tx.clone();
                    let active = active.clone();

                    // Just update all the workspaces
                    event_listener.$event(move |_state| {
                        Workspaces::get().unwrap().into_iter().for_each(|ws| {
                            let prev_workspace = lock!(active);
                            let focused = prev_workspace
                                .as_ref()
                                .map_or(Visibility::Visible(false), |w| {
                                    Visibility::Visible(w.id == WorkspaceId(format!("{}", ws.id)))
                                });
                            send!(tx, WorkspaceUpdate::Update(Workspace::from((focused, ws))));
                        });
                    })
                };
            }

            workspace_batch_event!(add_window_open_handler);
            workspace_batch_event!(add_window_close_handler);
            workspace_batch_event!(add_window_moved_handler);

            {
                let tx = tx.clone();
                let lock = lock.clone();
                let active = active.clone();

                event_listener.add_active_monitor_change_handler(move |event_data| {
                    let _lock = lock!(lock);
                    let workspace_type = event_data.workspace;

                    let mut prev_workspace = lock!(active);

                    debug!(
                        "Received active monitor change: {:?} -> {workspace_type:?}",
                        prev_workspace.as_ref().map(|w| &w.name)
                    );

                    let workspace_name = get_workspace_id(workspace_type);
                    let workspace = Self::get_workspace(&workspace_name, prev_workspace.as_ref());

                    if let Some((false, workspace)) =
                        workspace.map(|w| (w.visibility.is_focused(), w))
                    {
                        Self::send_focus_change(&mut prev_workspace, workspace, &tx);
                    } else {
                        error!("Unable to locate workspace");
                    }
                });
            }

            {
                let tx = tx.clone();
                let lock = lock.clone();

                event_listener.add_workspace_moved_handler(move |event_data| {
                    let _lock = lock!(lock);
                    let workspace_type = event_data.workspace;
                    debug!("Received workspace move: {workspace_type:?}");

                    let mut prev_workspace = lock!(active);

                    let workspace_name = get_workspace_id(workspace_type);
                    let workspace = Self::get_workspace(&workspace_name, prev_workspace.as_ref());

                    if let Some(workspace) = workspace {
                        send!(tx, WorkspaceUpdate::Move(workspace.clone()));

                        if !workspace.visibility.is_focused() {
                            Self::send_focus_change(&mut prev_workspace, workspace, &tx);
                        }
                    }
                });
            }

            {
                event_listener.add_workspace_destroy_handler(move |workspace_type| {
                    let _lock = lock!(lock);
                    debug!("Received workspace destroy: {workspace_type:?}");

                    let name = get_workspace_id(workspace_type);
                    debug!("Received workspace destroy: {name:?}");

                    // TODO: Horrible hack, see other todo in remove handler
                    send!(tx, WorkspaceUpdate::Remove { name: name.0 });
                });
            }

            event_listener
                .start_listener()
                .expect("Failed to start listener");
        });
    }

    /// Sends a `WorkspaceUpdate::Focus` event
    /// and updates the active workspace cache.
    fn send_focus_change(
        prev_workspace: &mut Option<Workspace>,
        workspace: Workspace,
        tx: &Sender<WorkspaceUpdate>,
    ) {
        let old = prev_workspace.as_ref();

        if let Some(old) = old {
            send!(
                tx,
                WorkspaceUpdate::Focus {
                    old: prev_workspace.take(),
                    new: workspace.clone(),
                }
            );
        }
        prev_workspace.replace(workspace);
    }

    /// Gets a workspace by name from the server, given the active workspace if known.
    fn get_workspace(id: &WorkspaceId, active: Option<&Workspace>) -> Option<Workspace> {
        Workspaces::get()
            .expect("Failed to get workspaces")
            .find_map(|w| {
                if &WorkspaceId(w.id.to_string()) == id {
                    let vis = Visibility::from((&w, active.map(|w| w.name.as_ref()), &|w| {
                        create_is_visible()(w)
                    }));

                    Some(Workspace::from((vis, w)))
                } else {
                    None
                }
            })
    }

    /// Gets the active workspace from the server.
    fn get_active_workspace() -> Result<Workspace> {
        let w = HWorkspace::get_active().map(|w| Workspace::from((Visibility::focused(), w)))?;
        Ok(w)
    }
}

impl WorkspaceClient for EventClient {
    fn focus(&self, id: String) -> Result<()> {
        let identifier = match id.parse::<i32>() {
            Ok(inum) => WorkspaceIdentifierWithSpecial::Id(inum),
            Err(_) => WorkspaceIdentifierWithSpecial::Name(&id),
        };

        Dispatch::call(DispatchType::Workspace(identifier))?;
        Ok(())
    }

    fn subscribe_workspace_change(&self) -> Receiver<WorkspaceUpdate> {
        let rx = self.workspace_tx.subscribe();

        {
            let tx = self.workspace_tx.clone();

            let active_id = HWorkspace::get_active().ok().map(|active| active.name);
            let is_visible = create_is_visible();

            let workspaces = Workspaces::get()
                .expect("Failed to get workspaces")
                .map(|w| {
                    let vis = Visibility::from((&w, active_id.as_deref(), &is_visible));

                    Workspace::from((vis, w))
                })
                .collect();

            send!(tx, WorkspaceUpdate::Init(workspaces));
        }

        rx
    }
}

lazy_static! {
    static ref CLIENT: EventClient = {
        let client = EventClient::new();
        client.listen_workspace_events();
        client
    };
}

pub fn get_client() -> &'static EventClient {
    &CLIENT
}

fn get_workspace_id(name: WorkspaceType) -> WorkspaceId {
    match name {
        WorkspaceType::Regular(name) => WorkspaceId(name),
        WorkspaceType::Special(name) => WorkspaceId(name.unwrap_or_default()),
    }
}

/// Creates a function which determines if a workspace is visible. This function makes a Hyprland call that allocates so it should be cached when possible, but it is only valid so long as workspaces do not change so it should not be stored long term
fn create_is_visible() -> impl Fn(&HWorkspace) -> bool {
    let monitors = hyprland::data::Monitors::get().map_or(Vec::new(), |ms| ms.to_vec());

    move |w| monitors.iter().any(|m| m.active_workspace.id == w.id)
}

impl From<(Visibility, HWorkspace)> for Workspace {
    fn from((visibility, workspace): (Visibility, HWorkspace)) -> Self {
        Self {
            id: WorkspaceId(workspace.id.to_string()),
            name: workspace.name,
            monitor: workspace.monitor,
            visibility,
        }
    }
}

impl<'a, 'f, F> From<(&'a HWorkspace, Option<&str>, F)> for Visibility
where
    F: FnOnce(&'f HWorkspace) -> bool,
    'a: 'f,
{
    fn from((workspace, active_name, is_visible): (&'a HWorkspace, Option<&str>, F)) -> Self {
        if Some(workspace.name.as_str()) == active_name {
            Self::focused()
        } else if is_visible(workspace) {
            Self::visible()
        } else {
            Self::Hidden
        }
    }
}
