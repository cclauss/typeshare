//! This is the command line tool for Typeshare. It is used to generate source files in other
//! languages based on Rust code.

use anyhow::{anyhow, Context};
use clap::{CommandFactory, Parser};
use clap_complete::Generator;
use config::Config;
use ignore::{overrides::OverrideBuilder, types::TypesBuilder, WalkBuilder};
use log::error;
use parse::{all_types, parse_input, parser_inputs};
use rayon::iter::ParallelBridge;
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::Path,
};
#[cfg(feature = "go")]
use typeshare_core::language::Go;
use typeshare_core::{
    language::{
        CrateName, GenericConstraints, Kotlin, Language, Scala, SupportedLanguage, Swift,
        TypeScript,
    },
    parser::ParsedData,
};
use writer::write_generated;

use crate::args::{Args, Command};

mod args;
mod config;
mod parse;
mod writer;

fn main() -> anyhow::Result<()> {
    flexi_logger::Logger::try_with_env()
        .unwrap()
        .start()
        .unwrap();

    let options = Args::parse();

    if let Some(options) = options.subcommand {
        match options {
            Command::Completions { shell } => {
                shell.generate(&mut Args::command(), &mut io::stdout().lock())
            }
        }
        // let shell = options
        //     .value_of_t::<clap_complete_command::Shell>("shell")
        //     .context("Missing shell argument")?;

        // let mut command = build_command();
        // shell.generate(&mut command, &mut std::io::stdout());
    }

    let config_file = options.config_file.as_deref();
    let config = config::load_config(config_file).context("Unable to read configuration file")?;
    let config = override_configuration(config, &options)?;

    if options.generate_config {
        let config = override_configuration(Config::default(), &options)?;
        config::store_config(&config, config_file).context("Failed to create new config file")?;
    }

    let directories = options.directories.as_slice();

    let language_type = match options.language {
        args::AvailableLanguage::Kotlin => SupportedLanguage::Kotlin,
        args::AvailableLanguage::Scala => SupportedLanguage::Scala,
        args::AvailableLanguage::Swift => SupportedLanguage::Swift,
        args::AvailableLanguage::Typescript => SupportedLanguage::TypeScript,
        #[cfg(feature = "go")]
        args::AvailableLanguage::Go => SupportedLanguage::Go,
    };

    let mut types = TypesBuilder::new();
    types
        .add("rust", "*.rs")
        .context("Failed to add rust type extensions")?;
    types.select("rust");

    // This is guaranteed to always have at least one value by the clap configuration
    let first_root = directories
        .first()
        .context("directories is empty; this shouldn't be possible")?;

    let overrides = OverrideBuilder::new(first_root)
        // Don't process files inside of tools/typeshare/
        .add("!**/tools/typeshare/**")
        .context("Failed to parse override")?
        .build()
        .context("Failed to build override")?;

    let mut walker_builder = WalkBuilder::new(first_root);
    // Sort walker output for deterministic output across platforms
    walker_builder.sort_by_file_path(Path::cmp);
    walker_builder.types(types.build().context("Failed to build types")?);
    walker_builder.overrides(overrides);
    walker_builder.follow_links(options.follow_links);

    for root in directories {
        walker_builder.add(root);
    }

    let multi_file = options.output_folder.is_some();
    let target_os = config.target_os.clone();

    let lang = language(language_type, config, multi_file);
    let ignored_types = lang.ignored_reference_types();

    // The walker ignores directories that are git-ignored. If you need
    // a git-ignored directory to be processed, add the specific directory to
    // the list of directories given to typeshare when it's invoked in the
    // makefiles
    // TODO: The `ignore` walker supports parallel walking. We should use this
    // and implement a `ParallelVisitor` that builds up the mapping of parsed
    // data. That way both walking and parsing are in parallel.
    // https://docs.rs/ignore/latest/ignore/struct.WalkParallel.html
    let crate_parsed_data = parse_input(
        parser_inputs(walker_builder, language_type, multi_file).par_bridge(),
        &ignored_types,
        multi_file,
        &target_os,
    )?;

    // Collect all the types into a map of the file name they
    // belong too and the list of type names. Used for generating
    // imports in generated files.
    let import_candidates = if multi_file {
        all_types(&crate_parsed_data)
    } else {
        HashMap::new()
    };

    check_parse_errors(&crate_parsed_data)?;
    write_generated(&options, lang, crate_parsed_data, import_candidates)?;

    Ok(())
}

/// Get the language trait impl for the given supported language and configuration.
fn language(
    language_type: SupportedLanguage,
    config: Config,
    multi_file: bool,
) -> Box<dyn Language> {
    match language_type {
        SupportedLanguage::Swift => Box::new(Swift {
            prefix: config.swift.prefix,
            type_mappings: config.swift.type_mappings,
            default_decorators: config.swift.default_decorators,
            default_generic_constraints: GenericConstraints::from_config(
                config.swift.default_generic_constraints,
            ),
            multi_file,
            codablevoid_constraints: config.swift.codablevoid_constraints,
            ..Default::default()
        }),
        SupportedLanguage::Kotlin => Box::new(Kotlin {
            package: config.kotlin.package,
            module_name: config.kotlin.module_name,
            prefix: config.kotlin.prefix,
            type_mappings: config.kotlin.type_mappings,
            ..Default::default()
        }),
        SupportedLanguage::Scala => Box::new(Scala {
            package: config.scala.package,
            module_name: config.scala.module_name,
            type_mappings: config.scala.type_mappings,
            ..Default::default()
        }),
        SupportedLanguage::TypeScript => Box::new(TypeScript {
            type_mappings: config.typescript.type_mappings,
            ..Default::default()
        }),
        #[cfg(feature = "go")]
        SupportedLanguage::Go => Box::new(Go {
            package: config.go.package,
            type_mappings: config.go.type_mappings,
            uppercase_acronyms: config.go.uppercase_acronyms,
            no_pointer_slice: config.go.no_pointer_slice,
            ..Default::default()
        }),
        #[cfg(not(feature = "go"))]
        SupportedLanguage::Go => {
            panic!("go support is currently experimental and must be enabled as a feature flag for typeshare-cli")
        }
    }
}

/// Overrides any configuration values with provided arguments
fn override_configuration(mut config: Config, options: &Args) -> anyhow::Result<Config> {
    if let Some(swift_prefix) = options.swift_prefix.as_ref() {
        config.swift.prefix = swift_prefix.clone();
    }

    if let Some(kotlin_prefix) = options.kotlin_prefix.as_ref() {
        config.kotlin.prefix = kotlin_prefix.clone();
    }

    if let Some(java_package) = options.java_package.as_ref() {
        config.kotlin.package = java_package.clone();
    }

    if let Some(module_name) = options.kotlin_module_name.as_ref() {
        config.kotlin.module_name = module_name.to_string();
    }

    if let Some(scala_package) = options.scala_package.as_ref() {
        config.scala.package = scala_package.clone();
    }

    if let Some(scala_module_name) = options.scala_module_name.as_ref() {
        config.scala.module_name = scala_module_name.to_string();
    }

    #[cfg(feature = "go")]
    {
        if let Some(go_package) = options.go_package.as_ref() {
            config.go.package = go_package.to_string();
        }
        assert_go_package_present(&config)?;
    }

    config.target_os = options.target_os.as_deref().unwrap_or_default().to_vec();

    Ok(config)
}

/// Prints out all parsing errors if any and returns Err.
fn check_parse_errors(parsed_crates: &BTreeMap<CrateName, ParsedData>) -> anyhow::Result<()> {
    let mut errors_encountered = false;
    for data in parsed_crates
        .values()
        .filter(|parsed_data| !parsed_data.errors.is_empty())
    {
        errors_encountered = true;
        for error in &data.errors {
            error!(
                "Parsing error: \"{}\" in crate \"{}\" for file \"{}\"",
                error.error, error.crate_name, error.file_name
            );
        }
    }

    if errors_encountered {
        Err(anyhow!("Errors encountered during parsing."))
    } else {
        Ok(())
    }
}

#[cfg(feature = "go")]
fn assert_go_package_present(config: &Config) -> anyhow::Result<()> {
    if config.go.package.is_empty() {
        return Err(anyhow!(
            "Please provide a package name in the typeshare.toml or using --go-package <package name>"
        ));
    }
    Ok(())
}
