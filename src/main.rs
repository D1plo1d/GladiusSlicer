#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use clap::{load_yaml, App};
use gladius_shared::loader::*;
use gladius_shared::types::*;

use crate::plotter::convert_objects_into_moves;
use crate::tower::*;
use geo::*;
use gladius_shared::settings::{PartialSettings, Settings};
use std::fs::File;

use std::ffi::OsStr;
use std::path::Path;

use crate::calculation::calculate_values;
use crate::command_pass::{CommandPass, OptimizePass, SlowDownLayerPass};
use crate::coverter::*;
use crate::input::files_input;
use crate::plotter::polygon_operations::PolygonOperations;
use crate::slice_pass::*;
use crate::slicing::*;
use crate::utils::{send_error_message, show_error_message};
use gladius_shared::error::SlicerErrors;
use gladius_shared::messages::Message;
use itertools::Itertools;
use log::{debug, info, LevelFilter};
use ordered_float::OrderedFloat;
use rayon::prelude::*;
use simple_logger::SimpleLogger;
use std::collections::HashMap;

mod calculation;
mod command_pass;
mod coverter;
mod input;
mod optimizer;
mod plotter;
mod slice_pass;
mod slicing;
mod tower;
mod utils;

fn main() {
    // The YAML file is found relative to the current file, similar to how modules are found
    let yaml = load_yaml!("cli.yaml");
    let matches = App::from_yaml(yaml).get_matches();

    //set number of cores for rayon
    if let Some(number_of_threads) = matches
        .value_of("THREAD_COUNT")
        .map(|str| str.parse::<usize>().ok())
        .flatten()
    {
        rayon::ThreadPoolBuilder::new()
            .num_threads(number_of_threads)
            .build_global()
            .unwrap();
    }

    let send_messages = matches.is_present("MESSAGES");

    if !send_messages {
        // Vary the output based on how many times the user used the "verbose" flag
        // (i.e. 'myprog -v -v -v' or 'myprog -vvv' vs 'myprog -v'
        match matches.occurrences_of("VERBOSE") {
            0 => SimpleLogger::new()
                .with_level(LevelFilter::Error)
                .init()
                .unwrap(),
            1 => SimpleLogger::new()
                .with_level(LevelFilter::Warn)
                .init()
                .unwrap(),
            2 => SimpleLogger::new()
                .with_level(LevelFilter::Info)
                .init()
                .unwrap(),
            3 => SimpleLogger::new()
                .with_level(LevelFilter::Debug)
                .init()
                .unwrap(),
            4 | _ => SimpleLogger::new()
                .with_level(LevelFilter::Trace)
                .init()
                .unwrap(),
        }
    }

    info!("Loading Inputs");
    let (models, settings) = handle_err_or_return(
        files_input(
            matches.value_of("SETTINGS"),
            matches
                .values_of("INPUT")
                .map(|values| values.map(|v| v.to_string()).collect()),
        ),
        send_messages,
    );

    info!("Creating Towers");

    let towers: Vec<TriangleTower> = handle_err_or_return(create_towers(&models), send_messages);

    info!("Slicing");

    let mut objects = slice(&towers, &settings);

    info!("Generating Moves");

    let mut moves = handle_err_or_return(generate_moves(objects, &settings), send_messages);

    debug!("Optimizing {} Moves", moves.len());

    OptimizePass::pass(&mut moves, &settings);

    SlowDownLayerPass::pass(&mut moves, &settings);

    if send_messages {
        let message = Message::Commands(moves.clone());
        println!("{}", serde_json::to_string(&message).unwrap());
    }

    let cv = calculate_values(&moves, &settings);

    if send_messages {
        let message = Message::CalculatedValues(cv);
        println!("{}", serde_json::to_string(&message).unwrap());
    } else {
        let (hour, min, sec, _) = cv.get_hours_minutes_seconds_fract_time();

        info!(
            "Total Time: {} hours {} minutes {:.3} seconds",
            hour, min, sec
        );
        info!(
            "Total Filament Volume: {:.3} cm^3",
            cv.plastic_volume / 1000.0
        );
        info!("Total Filament Mass: {:.3} grams", cv.plastic_weight);
        info!("Total Filament Length: {:.3} grams", cv.plastic_length);
        info!(
            "Total Filament Cost: {:.2} $",
            (((cv.plastic_volume / 1000.0) * settings.filament.density) / 1000.0)
                * settings.filament.cost
        );
    }

    //Output the GCode
    if let Some(file_path) = matches.value_of("OUTPUT") {
        //Output to file
        debug!("Converting {} Moves", moves.len());
        convert(
            &moves,
            settings,
            &mut File::create(file_path).expect("File not Found"),
        )
        .unwrap();
    } else if send_messages {
        //Output as message
        let mut gcode: Vec<u8> = Vec::new();
        convert(&moves, settings, &mut gcode).unwrap();
        let message = Message::GCode(String::from_utf8(gcode).unwrap());
        println!("{}", serde_json::to_string(&message).unwrap());
    } else {
        //Output to stdout
        let stdout = std::io::stdout();
        debug!("Converting {} Moves", moves.len());
        convert(&moves, settings, &mut stdout.lock()).unwrap();
    };
}

fn generate_moves(
    mut objects: Vec<Object>,
    settings: &Settings,
) -> Result<Vec<Command>, SlicerErrors> {
    //Creates Support Towers
    SupportTowerPass::pass(&mut objects, &settings);

    //Adds a skirt
    SkirtPass::pass(&mut objects, &settings);

    //Adds a brim
    BrimPass::pass(&mut objects, &settings);

    objects.par_iter_mut().for_each(|object| {
        let slices = &mut object.layers;

        //Shrink layer
        ShrinkPass::pass(slices, &settings);

        //Handle Perimeters
        PerimeterPass::pass(slices, &settings);

        //Handle Bridging
        BridgingPass::pass(slices, &settings);

        //Handle Top Layer
        TopLayerPass::pass(slices, &settings);

        //Handle Top And Bottom Layers
        TopAndBottomLayersPass::pass(slices, &settings);

        //Handle Support
        SupportPass::pass(slices, &settings);

        //Lightning Infill
        LightningFillPass::pass(slices, &settings);

        //Fill Remaining areas
        FillAreaPass::pass(slices, &settings);

        //Order the move chains
        OrderPass::pass(slices, &settings);
    });

    Ok(convert_objects_into_moves(objects, &settings))
}

fn handle_err_or_return<T>(res: Result<T, SlicerErrors>, send_message: bool) -> T {
    match res {
        Ok(data) => data,
        Err(slicer_error) => {
            if send_message {
                send_error_message(slicer_error)
            } else {
                show_error_message(slicer_error)
            }
            std::process::exit(-1);
        }
    }
}
