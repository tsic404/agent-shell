//! GNOME backend 装配器（§4.2–4.3、§3.3 装配矩阵 GNOME 行、§21.6）。

pub mod services;

pub use agent_shell_power::{probe_first_existing, service_exists};

mod assemble;

pub use assemble::{GnomeBackend, SessionType};
