use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

#[path = "contract/support.rs"]
mod support;
use support::*;

#[path = "contract/cli.rs"]
mod cli;
#[path = "contract/commands.rs"]
mod commands;
#[path = "contract/compaction.rs"]
mod compaction;
#[path = "contract/mcp.rs"]
mod mcp;
#[path = "contract/permissions.rs"]
mod permissions;
#[path = "contract/providers.rs"]
mod providers;
#[path = "contract/sessions.rs"]
mod sessions;
#[path = "contract/streaming.rs"]
mod streaming;
