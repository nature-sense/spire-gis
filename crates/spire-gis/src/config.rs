// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! spire-gis configuration helpers.

use std::path::PathBuf;

/// User-level data directory for the GIS graph store (mirrors
/// `spire_core::config::knowledge_dir`): the datasets/Layers/Features the app
/// imports live here, independent of any single project.
///
/// Override with `SPIRE_GIS_DATA_DIR`; default `~/.spire/gis-data`.
pub fn gis_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SPIRE_GIS_DATA_DIR") {
        let trimmed = dir.trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    home.join(".spire").join("gis-data")
}
