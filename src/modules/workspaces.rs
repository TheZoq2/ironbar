use crate::clients::compositor::{Compositor, Visibility, Workspace, WorkspaceUpdate, WorkspaceId};
use crate::config::CommonConfig;
use crate::image::new_icon_button;
use crate::modules::{Module, ModuleInfo, ModuleParts, ModuleUpdateEvent, WidgetContext};
use crate::{send_async, try_send};
use color_eyre::{Report, Result};
use gtk::{prelude::*, Label};
use gtk::{Button, IconTheme};
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use tokio::spawn;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::trace;

#[derive(Debug, Deserialize, Clone, Copy, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Shows workspaces in the order they're added
    Added,
    /// Shows workspaces in numeric order.
    /// Named workspaces are added to the end in alphabetical order.
    Alphanumeric,
}

impl Default for SortOrder {
    fn default() -> Self {
        Self::Alphanumeric
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Favorites {
    ByMonitor(HashMap<String, Vec<String>>),
    Global(Vec<String>),
}

impl Default for Favorites {
    fn default() -> Self {
        Self::Global(vec![])
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct WorkspacesModule {
    /// Map of actual workspace names to custom names.
    name_map: Option<HashMap<String, String>>,

    /// Array of always shown workspaces, and what monitor to show on
    #[serde(default)]
    favorites: Favorites,

    /// List of workspace names to never show
    #[serde(default)]
    hidden: Vec<String>,

    /// Whether to display buttons for all monitors.
    #[serde(default = "crate::config::default_false")]
    all_monitors: bool,

    #[serde(default)]
    sort: SortOrder,

    #[serde(default = "default_icon_size")]
    icon_size: i32,

    #[serde(flatten)]
    pub common: Option<CommonConfig>,
}

const fn default_icon_size() -> i32 {
    32
}

/// Creates a button from a workspace
fn create_button(
    name: &str,
    visibility: Visibility,
    name_map: &HashMap<String, String>,
    icon_theme: &IconTheme,
    icon_size: i32,
    tx: &Sender<String>,
) -> Button {
    let button = Button::new();
    let label = Label::builder()
        .use_markup(true)
        .vexpand(true)
        .vexpand_set(true)
        .valign(gtk::Align::Start)
        .label(name.replace("$NL$", "\n"));
    button.set_child(Some(&label.build()));
    button.set_widget_name(name);

    let style_context = button.style_context();
    style_context.add_class("item");

    if visibility.is_visible() {
        style_context.add_class("visible");
    }

    if visibility.is_focused() {
        style_context.add_class("focused");
    }

    {
        let tx = tx.clone();
        let name = name.to_string();
        button.connect_clicked(move |_item| {
            try_send!(tx, name.clone());
        });
    }

    button
}

fn reorder_workspaces(container: &gtk::Box) {
    let mut buttons = container
        .children()
        .into_iter()
        .map(|child| (child.widget_name().to_string(), child))
        .collect::<Vec<_>>();

    buttons.sort_by(|(label_a, _), (label_b, _a)| {
        label_a.cmp(label_b)
        // match (label_a.parse::<i32>(), label_b.parse::<i32>()) {
        //     (Ok(a), Ok(b)) => a.cmp(&b),
        //     (Ok(_), Err(_)) => Ordering::Less,
        //     (Err(_), Ok(_)) => Ordering::Greater,
        //     (Err(_), Err(_)) => label_a.cmp(label_b),
        // }
    });

    for (i, (_, button)) in buttons.into_iter().enumerate() {
        container.reorder_child(&button, i as i32);
    }
}

impl WorkspacesModule {
    fn show_workspace_check(&self, output: &String, work: &Workspace) -> bool {
        (work.visibility.is_focused() || !self.hidden.contains(&work.name))
            && (self.all_monitors || output == &work.monitor)
    }
}

impl Module<gtk::Box> for WorkspacesModule {
    type SendMessage = WorkspaceUpdate;
    type ReceiveMessage = String;

    fn name() -> &'static str {
        "workspaces"
    }

    fn spawn_controller(
        &self,
        _info: &ModuleInfo,
        tx: Sender<ModuleUpdateEvent<Self::SendMessage>>,
        mut rx: Receiver<Self::ReceiveMessage>,
    ) -> Result<()> {
        // Subscribe & send events
        spawn(async move {
            let mut srx = {
                let client =
                    Compositor::get_workspace_client().expect("Failed to get workspace client");
                client.subscribe_workspace_change()
            };

            trace!("Set up Sway workspace subscription");

            while let Ok(payload) = srx.recv().await {
                send_async!(tx, ModuleUpdateEvent::Update(payload));
            }
        });

        // Change workspace focus
        spawn(async move {
            trace!("Setting up UI event handler");

            while let Some(name) = rx.recv().await {
                let client =
                    Compositor::get_workspace_client().expect("Failed to get workspace client");
                client.focus(name)?;
            }

            Ok::<(), Report>(())
        });

        Ok(())
    }

    fn into_widget(
        self,
        context: WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        info: &ModuleInfo,
    ) -> Result<ModuleParts<gtk::Box>> {
        let container = gtk::Box::new(info.bar_position.get_orientation(), 0);

        let name_map = self.name_map.clone().unwrap_or_default();
        let favs = self.favorites.clone();
        let mut fav_names: Vec<String> = vec![];

        let mut button_map: HashMap<WorkspaceId, Button> = HashMap::new();

        {
            let container = container.clone();
            let output_name = info.output_name.to_string();
            let icon_theme = info.icon_theme.clone();
            let icon_size = self.icon_size;

            // keep track of whether init event has fired previously
            // since it fires for every workspace subscriber
            let mut has_initialized = false;

            context.widget_rx.attach(None, move |event| {
                match event {
                    WorkspaceUpdate::Init(workspaces) => {
                        if !has_initialized {
                            trace!("Creating workspace buttons");

                            let mut added = HashSet::new();

                            let mut add_workspace = |id: &WorkspaceId, name: &str, visibility: Visibility| {
                                let item = create_button(
                                    name,
                                    visibility,
                                    &name_map,
                                    &icon_theme,
                                    icon_size,
                                    &context.controller_tx,
                                );

                                container.add(&item);
                                button_map.insert(id.clone(), item);
                            };

                            // add workspaces from client
                            for workspace in &workspaces {
                                if self.show_workspace_check(&output_name, workspace) {
                                    add_workspace(&workspace.id, &workspace.name, workspace.visibility);
                                    added.insert(workspace.name.to_string());
                                }
                            }

                            let mut add_favourites = |names: &Vec<String>| {
                                // TODO: Re-add support for favourites
                                // for name in names {
                                //     if !added.contains(name) {
                                //         add_workspace(name, Visibility::Hidden);
                                //         added.insert(name.to_string());
                                //         fav_names.push(name.to_string());
                                //     }
                                // }
                            };

                            // add workspaces from favourites
                            match &favs {
                                Favorites::Global(names) => add_favourites(names),
                                Favorites::ByMonitor(map) => {
                                    if let Some(to_add) = map.get(&output_name) {
                                        add_favourites(to_add);
                                    }
                                }
                            }

                            if self.sort == SortOrder::Alphanumeric {
                                reorder_workspaces(&container);
                            }

                            container.show_all();
                            has_initialized = true;
                        }
                    }
                    WorkspaceUpdate::Focus { old, new } => {
                        if let Some(btn) = old.as_ref().and_then(|w| button_map.get(&w.id)) {
                            if Some(new.monitor) == old.map(|w| w.monitor) {
                                btn.style_context().remove_class("visible");
                            }

                            btn.style_context().remove_class("focused");
                        }

                        let new = button_map.get(&new.id);
                        if let Some(btn) = new {
                            let style = btn.style_context();

                            style.add_class("visible");
                            style.add_class("focused");
                        }
                    }
                    WorkspaceUpdate::Update(workspace) => {
                        println!("Workspace update {workspace:?}");
                        let old = button_map.get(&workspace.id);
                        if let Some(item) = old {
                            container.remove(item);
                        }

                        if self.all_monitors || workspace.monitor == output_name {
                            let item = create_button(
                                &workspace.name,
                                workspace.visibility,
                                &name_map,
                                &icon_theme,
                                icon_size,
                                &context.controller_tx,
                            );

                            container.add(&item);

                            if self.sort == SortOrder::Alphanumeric {
                                reorder_workspaces(&container);
                            }

                            item.show();

                            if !workspace.id.0.is_empty() {
                                button_map.insert(workspace.id, item);
                            }

                            container.show_all();
                        }
                    }
                    WorkspaceUpdate::Add(workspace) => {
                        if fav_names.contains(&workspace.name) {
                            let btn = button_map.get(&workspace.id);
                            if let Some(btn) = btn {
                                btn.style_context().remove_class("inactive");
                            }
                        } else if self.show_workspace_check(&output_name, &workspace) {
                            let name = workspace.name;
                            let item = create_button(
                                &name,
                                workspace.visibility,
                                &name_map,
                                &icon_theme,
                                icon_size,
                                &context.controller_tx,
                            );

                            container.add(&item);
                            if self.sort == SortOrder::Alphanumeric {
                                reorder_workspaces(&container);
                            }

                            item.show();

                            if !workspace.id.0.is_empty() {
                                button_map.insert(workspace.id, item);
                            }
                        }
                    }
                    WorkspaceUpdate::Move(workspace) => {
                        if !self.hidden.contains(&workspace.name) && !self.all_monitors {
                            if workspace.monitor == output_name {
                                let name = workspace.name;
                                let item = create_button(
                                    &name,
                                    workspace.visibility,
                                    &name_map,
                                    &icon_theme,
                                    icon_size,
                                    &context.controller_tx,
                                );

                                container.add(&item);

                                if self.sort == SortOrder::Alphanumeric {
                                    reorder_workspaces(&container);
                                }

                                item.show();

                                if !name.is_empty() {
                                    button_map.insert(workspace.id, item);
                                }
                            } else if let Some(item) = button_map.get(&workspace.id) {
                                container.remove(item);
                            }
                        }
                    }
                    WorkspaceUpdate::Remove{name} => {
                        // NOTE: Workspace remove is unsupported
                        println!("Workspace remove event");
                        // TODO: This is super cursed, we're removing workspaces by
                        // name here, but the button map contains IDs* However,
                        // by the time a workspace is removed, it is empty and so
                        // its name matches its id
                        let button = button_map.get(&WorkspaceId(name.clone()));
                        if let Some(item) = button {
                            if fav_names.contains(&name) {
                                item.style_context().add_class("inactive");
                            } else {
                                container.remove(item);
                            }
                        }
                    }
                };

                Continue(true)
            });
        }

        Ok(ModuleParts {
            widget: container,
            popup: None,
        })
    }
}
