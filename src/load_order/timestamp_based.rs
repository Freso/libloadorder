/*
 * This file is part of libloadorder
 *
 * Copyright (C) 2017 Oliver Hamlet
 *
 * libloadorder is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * libloadorder is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with libloadorder. If not, see <http://www.gnu.org/licenses/>.
 */
use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use encoding::all::WINDOWS_1252;
use encoding::{EncoderTrap, Encoding};
use rayon::prelude::*;
use regex::Regex;

use super::insertable::InsertableLoadOrder;
use super::mutable::{load_active_plugins, MutableLoadOrder};
use super::readable::{
    active_plugin_names, index_of, is_active, plugin_at, plugin_names, ReadableLoadOrder,
    ReadableLoadOrderExt,
};
use super::writable::{activate, deactivate, set_active_plugins, WritableLoadOrder};
use super::{create_parent_dirs, find_first_non_master_position};
use enums::{Error, GameId};
use game_settings::GameSettings;
use plugin::Plugin;

const GAME_FILES_HEADER: &[u8] = b"[Game Files]";

#[derive(Clone, Debug)]
pub struct TimestampBasedLoadOrder {
    game_settings: GameSettings,
    plugins: Vec<Plugin>,
}

impl TimestampBasedLoadOrder {
    pub fn new(game_settings: GameSettings) -> Self {
        Self {
            game_settings,
            plugins: Vec::new(),
        }
    }
}

impl ReadableLoadOrder for TimestampBasedLoadOrder {
    fn game_settings(&self) -> &GameSettings {
        &self.game_settings
    }

    fn plugin_names(&self) -> Vec<&str> {
        plugin_names(self.plugins())
    }

    fn index_of(&self, plugin_name: &str) -> Option<usize> {
        index_of(self.plugins(), plugin_name)
    }

    fn plugin_at(&self, index: usize) -> Option<&str> {
        plugin_at(self.plugins(), index)
    }

    fn active_plugin_names(&self) -> Vec<&str> {
        active_plugin_names(self.plugins())
    }

    fn is_active(&self, plugin_name: &str) -> bool {
        is_active(self.plugins(), plugin_name)
    }
}

impl ReadableLoadOrderExt for TimestampBasedLoadOrder {
    fn plugins(&self) -> &Vec<Plugin> {
        &self.plugins
    }
}

impl MutableLoadOrder for TimestampBasedLoadOrder {
    fn plugins_mut(&mut self) -> &mut Vec<Plugin> {
        &mut self.plugins
    }
}

impl InsertableLoadOrder for TimestampBasedLoadOrder {
    fn insert_position(&self, plugin: &Plugin) -> Option<usize> {
        if plugin.is_master_file() {
            find_first_non_master_position(self.plugins())
        } else {
            None
        }
    }
}

impl WritableLoadOrder for TimestampBasedLoadOrder {
    fn load(&mut self) -> Result<(), Error> {
        self.plugins_mut().clear();

        self.plugins = load_plugins_from_dir(self);
        self.plugins.par_sort_by(plugin_sorter);

        let regex = Regex::new(r"(?i)GameFile[0-9]{1,3}=(.+\.es(?:m|p))")?;
        let game_id = self.game_settings().id();
        let line_mapper = |line: &str| plugin_line_mapper(line, &regex, game_id);

        load_active_plugins(self, line_mapper)?;

        self.add_implicitly_active_plugins()?;

        self.deactivate_excess_plugins();

        Ok(())
    }

    fn save(&mut self) -> Result<(), Error> {
        let timestamps = padded_unique_timestamps(self.plugins());

        let result: Result<Vec<()>, Error> = self.plugins_mut()
            .par_iter_mut()
            .zip(timestamps.into_par_iter())
            .map(|(ref mut plugin, timestamp)| plugin.set_modification_time(timestamp))
            .collect();

        match result {
            Ok(_) => save_active_plugins(self),
            Err(e) => Err(e),
        }
    }

    fn set_load_order(&mut self, plugin_names: &[&str]) -> Result<(), Error> {
        self.replace_plugins(plugin_names)
    }

    fn set_plugin_index(&mut self, plugin_name: &str, position: usize) -> Result<(), Error> {
        self.move_or_insert_plugin_with_index(plugin_name, position)
    }

    fn is_self_consistent(&self) -> Result<bool, Error> {
        Ok(true)
    }

    fn activate(&mut self, plugin_name: &str) -> Result<(), Error> {
        activate(self, plugin_name)
    }

    fn deactivate(&mut self, plugin_name: &str) -> Result<(), Error> {
        deactivate(self, plugin_name)
    }

    fn set_active_plugins(&mut self, active_plugin_names: &[&str]) -> Result<(), Error> {
        set_active_plugins(self, active_plugin_names)
    }
}

fn load_plugins_from_dir<T: ReadableLoadOrderExt>(load_order: &T) -> Vec<Plugin> {
    let filenames = load_order.find_plugins_in_dir();
    let game_settings = load_order.game_settings();

    filenames
        .par_iter()
        .filter_map(|f| Plugin::new(&f, game_settings).ok())
        .collect()
}

fn plugin_sorter(a: &Plugin, b: &Plugin) -> Ordering {
    if a.is_master_file() == b.is_master_file() {
        match a.modification_time().cmp(&b.modification_time()) {
            Ordering::Equal => a.name().cmp(&b.name()),
            x => x,
        }
    } else if a.is_master_file() {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

fn plugin_line_mapper(mut line: &str, regex: &Regex, game_id: GameId) -> Option<String> {
    if game_id == GameId::Morrowind {
        line = regex
            .captures(&line)
            .and_then(|c| c.get(1))
            .map_or(&line[0..0], |m| m.as_str());
    }

    if line.is_empty() || line.starts_with('#') {
        None
    } else {
        Some(line.to_owned())
    }
}

fn padded_unique_timestamps(plugins: &[Plugin]) -> Vec<SystemTime> {
    let mut timestamps: Vec<SystemTime> = plugins.iter().map(Plugin::modification_time).collect();

    timestamps.sort();
    timestamps.dedup();

    while timestamps.len() < plugins.len() {
        let timestamp = *timestamps.last().unwrap_or(&UNIX_EPOCH) + Duration::from_secs(60);
        timestamps.push(timestamp);
    }

    timestamps
}

fn save_active_plugins<T: ReadableLoadOrderExt>(load_order: &mut T) -> Result<(), Error> {
    create_parent_dirs(load_order.game_settings().active_plugins_file())?;

    let prelude = get_file_prelude(load_order.game_settings())?;

    let file = File::create(&load_order.game_settings().active_plugins_file())?;
    let mut writer = BufWriter::new(file);
    writer.write_all(&prelude)?;
    for (index, plugin_name) in load_order.active_plugin_names().iter().enumerate() {
        if load_order.game_settings().id() == GameId::Morrowind {
            write!(writer, "GameFile{}=", index)?;
        }
        writer.write_all(&WINDOWS_1252
            .encode(plugin_name, EncoderTrap::Strict)
            .map_err(Error::EncodeError)?)?;
        writeln!(writer)?;
    }

    Ok(())
}

fn get_file_prelude(game_settings: &GameSettings) -> Result<Vec<u8>, Error> {
    let mut prelude: Vec<u8> = Vec::new();
    if game_settings.id() == GameId::Morrowind && game_settings.active_plugins_file().exists() {
        let input = File::open(game_settings.active_plugins_file())?;
        let buffered = BufReader::new(input);

        for line in buffered.split(b'\n') {
            let line = line?;
            prelude.append(&mut line.clone());
            prelude.push(b'\n');

            if line.starts_with(GAME_FILES_HEADER) {
                break;
            }
        }
    }

    Ok(prelude)
}

#[cfg(test)]
mod tests {
    use super::*;

    use enums::GameId;
    use filetime::{set_file_times, FileTime};
    use load_order::tests::*;
    use std::fs::{remove_dir_all, File};
    use std::io::{Read, Write};
    use std::path::Path;
    use tempfile::tempdir;
    use tests::copy_to_test_dir;

    fn prepare(game_id: GameId, game_dir: &Path) -> TimestampBasedLoadOrder {
        let (game_settings, plugins) = mock_game_files(game_id, game_dir);
        TimestampBasedLoadOrder {
            game_settings,
            plugins,
        }
    }

    fn write_file(path: &Path) {
        let mut file = File::create(&path).unwrap();
        writeln!(file, "").unwrap();
    }

    #[test]
    fn insert_position_should_return_none_if_given_a_non_master_plugin() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        let plugin =
            Plugin::new("Blank - Master Dependent.esp", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert_eq!(None, position);
    }

    #[test]
    fn insert_position_should_return_the_first_non_master_plugin_index_if_given_a_master_plugin() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        let plugin = Plugin::new("Blank.esm", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert_eq!(1, position.unwrap());
    }

    #[test]
    fn insert_position_should_return_none_if_no_non_masters_are_present() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        // Remove non-master plugins from the load order.
        load_order.plugins_mut().retain(|p| p.is_master_file());

        let plugin = Plugin::new("Blank.esm", &load_order.game_settings()).unwrap();
        let position = load_order.insert_position(&plugin);

        assert_eq!(None, position);
    }

    #[test]
    fn load_should_reload_existing_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        assert!(!load_order.plugins()[1].is_master_file());
        copy_to_test_dir("Blank.esm", "Blank.esp", &load_order.game_settings());
        let plugin_path = load_order
            .game_settings()
            .plugins_directory()
            .join("Blank.esp");
        set_file_times(&plugin_path, FileTime::zero(), FileTime::zero()).unwrap();

        load_order.load().unwrap();

        assert!(load_order.plugins()[1].is_master_file());
    }

    #[test]
    fn load_should_remove_plugins_that_fail_to_load() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        assert!(load_order.index_of("Blank.esp").is_some());
        assert!(load_order.index_of("Blank - Different.esp").is_some());

        let plugin_path = load_order
            .game_settings()
            .plugins_directory()
            .join("Blank.esp");
        write_file(&plugin_path);
        set_file_times(&plugin_path, FileTime::zero(), FileTime::zero()).unwrap();

        let plugin_path = load_order
            .game_settings()
            .plugins_directory()
            .join("Blank - Different.esp");
        write_file(&plugin_path);
        set_file_times(&plugin_path, FileTime::zero(), FileTime::zero()).unwrap();

        load_order.load().unwrap();
        assert!(load_order.index_of("Blank.esp").is_none());
        assert!(load_order.index_of("Blank - Different.esp").is_none());
    }

    #[test]
    fn load_should_sort_installed_plugins_into_their_timestamp_order_with_master_files_first() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        set_timestamps(
            &load_order.game_settings().plugins_directory(),
            &[
                "Blank - Master Dependent.esp",
                "Blank.esm",
                "Blank - Different.esp",
                "Blank.esp",
                load_order.game_settings().master_file(),
            ],
        );

        load_order.load().unwrap();

        let expected_filenames = vec![
            "Blank.esm",
            load_order.game_settings().master_file(),
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
            "Blank.esp",
            "Blàñk.esp",
        ];

        assert_eq!(expected_filenames, load_order.plugin_names());
    }

    #[test]
    fn load_should_empty_the_load_order_if_the_plugins_directory_does_not_exist() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());
        tmp_dir.close().unwrap();

        load_order.load().unwrap();

        assert!(load_order.plugins().is_empty());
    }

    #[test]
    fn load_should_decode_active_plugins_file_from_windows_1252() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        write_active_plugins_file(load_order.game_settings(), &["Blàñk.esp", "Blank.esm"]);

        load_order.load().unwrap();
        let expected_filenames = vec!["Blank.esm", "Blàñk.esp"];

        assert_eq!(expected_filenames, load_order.active_plugin_names());
    }

    #[test]
    fn load_should_handle_crlf_and_lf_in_active_plugins_file() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        write_active_plugins_file(load_order.game_settings(), &["Blàñk.esp", "Blank.esm\r"]);

        load_order.load().unwrap();
        let expected_filenames = vec!["Blank.esm", "Blàñk.esp"];

        assert_eq!(expected_filenames, load_order.active_plugin_names());
    }

    #[test]
    fn load_should_ignore_active_plugins_file_lines_starting_with_a_hash_for_oblivion() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        write_active_plugins_file(
            load_order.game_settings(),
            &["#Blank.esp", "Blàñk.esp", "Blank.esm"],
        );

        load_order.load().unwrap();
        let expected_filenames = vec!["Blank.esm", "Blàñk.esp"];

        assert_eq!(expected_filenames, load_order.active_plugin_names());
    }

    #[test]
    fn load_should_ignore_plugins_in_active_plugins_file_that_are_not_installed() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        write_active_plugins_file(
            load_order.game_settings(),
            &["Blàñk.esp", "Blank.esm", "missing.esp"],
        );

        load_order.load().unwrap();
        let expected_filenames = vec!["Blank.esm", "Blàñk.esp"];

        assert_eq!(expected_filenames, load_order.active_plugin_names());
    }

    #[test]
    fn load_should_load_plugin_states_from_active_plugins_file_for_oblivion() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        write_active_plugins_file(load_order.game_settings(), &["Blàñk.esp", "Blank.esm"]);

        load_order.load().unwrap();
        let expected_filenames = vec!["Blank.esm", "Blàñk.esp"];

        assert_eq!(expected_filenames, load_order.active_plugin_names());
    }

    #[test]
    fn load_should_succeed_when_active_plugins_file_is_missing() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        assert!(load_order.load().is_ok());
        assert!(load_order.active_plugin_names().is_empty());
    }

    #[test]
    fn load_should_load_plugin_states_from_active_plugins_file_for_morrowind() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        write_active_plugins_file(load_order.game_settings(), &["Blàñk.esp", "Blank.esm"]);

        load_order.load().unwrap();
        let expected_filenames = vec!["Blank.esm", "Blàñk.esp"];

        assert_eq!(expected_filenames, load_order.active_plugin_names());
    }

    #[test]
    fn load_should_deactivate_excess_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        let mut plugins: Vec<String> = Vec::new();
        plugins.push(load_order.game_settings().master_file().to_string());
        for i in 0..260 {
            plugins.push(format!("Blank{}.esm", i));
            copy_to_test_dir(
                "Blank.esm",
                &plugins.last().unwrap(),
                load_order.game_settings(),
            );
        }

        {
            let plugins_as_ref: Vec<&str> = plugins.iter().map(AsRef::as_ref).collect();
            write_active_plugins_file(load_order.game_settings(), &plugins_as_ref);
            set_timestamps(
                &load_order.game_settings().plugins_directory(),
                &plugins_as_ref,
            );
        }

        plugins = plugins[0..255].to_vec();

        load_order.load().unwrap();
        let active_plugin_names = load_order.active_plugin_names();

        assert_eq!(255, active_plugin_names.len());
        for i in 0..255 {
            assert_eq!(plugins[i], active_plugin_names[i]);
        }
        assert_eq!(plugins, active_plugin_names);
    }

    #[test]
    fn save_should_preserve_the_existing_set_of_timestamps() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        let mapper = |p: &Plugin| {
            p.modification_time()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };

        set_timestamps(
            &load_order.game_settings().plugins_directory(),
            &[
                "Blank - Master Dependent.esp",
                "Blank.esm",
                "Blank - Different.esp",
                "Blank.esp",
                load_order.game_settings().master_file(),
            ],
        );

        load_order.load().unwrap();

        let mut old_timestamps: Vec<u64> = load_order.plugins().iter().map(&mapper).collect();
        old_timestamps.sort();

        load_order.save().unwrap();

        let timestamps: Vec<u64> = load_order.plugins().iter().map(&mapper).collect();

        assert_eq!(old_timestamps, timestamps);
    }

    #[test]
    fn save_should_deduplicate_plugin_timestamps() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        let mapper = |p: &Plugin| {
            p.modification_time()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };

        set_timestamps(
            &load_order.game_settings().plugins_directory(),
            &[
                "Blank - Master Dependent.esp",
                "Blank.esm",
                "Blank - Different.esp",
                "Blank.esp",
                load_order.game_settings().master_file(),
            ],
        );

        // Give two files the same timestamp.
        load_order.plugins_mut()[1]
            .set_modification_time(UNIX_EPOCH + Duration::new(2, 0))
            .unwrap();

        load_order.load().unwrap();

        let mut old_timestamps: Vec<u64> = load_order.plugins().iter().map(&mapper).collect();

        load_order.save().unwrap();

        let timestamps: Vec<u64> = load_order.plugins().iter().map(&mapper).collect();

        assert_ne!(old_timestamps, timestamps);

        old_timestamps.sort();
        old_timestamps.dedup_by_key(|t| *t);
        let last_timestamp = *old_timestamps.last().unwrap();
        old_timestamps.push(last_timestamp + 60);

        assert_eq!(old_timestamps, timestamps);
    }

    #[test]
    fn save_should_create_active_plugins_file_parent_directory_if_it_does_not_exist() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        remove_dir_all(
            load_order
                .game_settings()
                .active_plugins_file()
                .parent()
                .unwrap(),
        ).unwrap();

        load_order.save().unwrap();

        assert!(
            load_order
                .game_settings()
                .active_plugins_file()
                .parent()
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn save_should_write_active_plugins_file_for_oblivion() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Oblivion, &tmp_dir.path());

        load_order.save().unwrap();

        load_order.load().unwrap();
        assert_eq!(vec!["Blank.esp"], load_order.active_plugin_names());
    }

    #[test]
    fn save_should_write_active_plugins_file_for_morrowind() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        write_active_plugins_file(load_order.game_settings(), &["Blàñk.esp", "Blank.esm"]);

        load_order.save().unwrap();

        load_order.load().unwrap();
        assert_eq!(vec!["Blank.esp"], load_order.active_plugin_names());

        let mut content = String::new();
        File::open(load_order.game_settings().active_plugins_file())
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(content.contains("isrealmorrowindini=false\n[Game Files]\n"));
    }

    #[test]
    fn set_load_order_should_error_if_given_duplicate_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        let filenames = vec!["Blank.esp", "blank.esp"];
        assert!(load_order.set_load_order(&filenames).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_load_order_should_error_if_given_an_invalid_plugin() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        let filenames = vec!["Blank.esp", "missing.esp"];
        assert!(load_order.set_load_order(&filenames).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_load_order_should_error_if_given_a_list_with_plugins_before_masters() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        let filenames = vec!["Blank.esp", "Blank.esm"];
        assert!(load_order.set_load_order(&filenames).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_load_order_should_not_distinguish_between_ghosted_and_unghosted_filenames() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        copy_to_test_dir(
            "Blank - Different.esm",
            "ghosted.esm.ghost",
            &load_order.game_settings(),
        );

        let filenames = vec![
            "Morrowind.esm",
            "Blank.esm",
            "ghosted.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
            "Blàñk.esp",
        ];

        assert!(load_order.set_load_order(&filenames).is_ok());
    }

    #[test]
    fn set_load_order_should_not_insert_missing_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let filenames = vec![
            "Blank.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
        ];
        load_order.set_load_order(&filenames).unwrap();

        assert_eq!(filenames, load_order.plugin_names());
    }

    #[test]
    fn set_load_order_should_not_lose_active_state_of_existing_plugins() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let filenames = vec![
            "Blank.esm",
            "Blank.esp",
            "Blank - Master Dependent.esp",
            "Blank - Different.esp",
        ];
        load_order.set_load_order(&filenames).unwrap();

        assert!(load_order.is_active("Blank.esp"));
    }

    #[test]
    fn set_plugin_index_should_error_if_inserting_a_non_master_before_a_master() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        assert!(
            load_order
                .set_plugin_index("Blank - Master Dependent.esp", 0)
                .is_err()
        );
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_plugin_index_should_error_if_moving_a_non_master_before_a_master() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        assert!(load_order.set_plugin_index("Blank.esp", 0).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_plugin_index_should_error_if_inserting_a_master_after_a_non_master() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        assert!(load_order.set_plugin_index("Blank.esm", 2).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_plugin_index_should_error_if_moving_a_master_after_a_non_master() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        assert!(load_order.set_plugin_index("Morrowind.esm", 2).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_plugin_index_should_error_if_setting_the_index_of_an_invalid_plugin() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let existing_filenames = to_owned(load_order.plugin_names());
        assert!(load_order.set_plugin_index("missing.esm", 0).is_err());
        assert_eq!(existing_filenames, load_order.plugin_names());
    }

    #[test]
    fn set_plugin_index_should_insert_a_new_plugin() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let num_plugins = load_order.plugins().len();
        load_order.set_plugin_index("Blank.esm", 1).unwrap();
        assert_eq!(1, load_order.index_of("Blank.esm").unwrap());
        assert_eq!(num_plugins + 1, load_order.plugins().len());
    }

    #[test]
    fn set_plugin_index_should_move_an_existing_plugin() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        let num_plugins = load_order.plugins().len();
        load_order
            .set_plugin_index("Blank - Different.esp", 1)
            .unwrap();
        assert_eq!(1, load_order.index_of("Blank - Different.esp").unwrap());
        assert_eq!(num_plugins, load_order.plugins().len());
    }

    #[test]
    fn set_plugin_index_should_move_an_existing_plugin_later_correctly() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        load_order
            .add_to_load_order("Blank - Master Dependent.esp")
            .unwrap();
        let num_plugins = load_order.plugins().len();
        load_order.set_plugin_index("Blank.esp", 2).unwrap();
        assert_eq!(2, load_order.index_of("Blank.esp").unwrap());
        assert_eq!(num_plugins, load_order.plugins().len());
    }

    #[test]
    fn set_plugin_index_should_preserve_an_existing_plugins_active_state() {
        let tmp_dir = tempdir().unwrap();
        let mut load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        load_order
            .add_to_load_order("Blank - Master Dependent.esp")
            .unwrap();
        load_order.set_plugin_index("Blank.esp", 2).unwrap();
        assert!(load_order.is_active("Blank.esp"));

        load_order
            .set_plugin_index("Blank - Different.esp", 2)
            .unwrap();
        assert!(!load_order.is_active("Blank - Different.esp"));
    }

    #[test]
    fn is_self_consistent_should_return_true() {
        let tmp_dir = tempdir().unwrap();
        let load_order = prepare(GameId::Morrowind, &tmp_dir.path());

        assert!(load_order.is_self_consistent().unwrap());
    }
}
