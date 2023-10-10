#[cfg(feature = "eidolonint")]
extern crate libeidolon;
#[macro_use]
extern crate structopt;
#[macro_use]
extern crate human_panic;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate log;
extern crate clap_verbosity_flag;
extern crate confy;
extern crate crc;
extern crate curl;
extern crate dialoguer;
extern crate dirs;
extern crate gog;
extern crate indicatif;
extern crate inflate;
extern crate rayon;
extern crate serde;
extern crate serde_json;
extern crate url;
extern crate walkdir;
extern crate zip;
extern crate anyhow;
mod args;
mod config;
mod connect;
mod games;
mod interactive;
mod sync;
use args::Command::Download;
use args::Command::*;
use args::Wyvern;
use args::{DownloadOptions, ShortcutOptions};

use anyhow::Result;
use config::*;
use crc::{Crc, CRC_32_ISCSI};
use dialoguer::*;
use games::*;
use gog::{extract::*, token::Token, Error, ErrorKind::*, Gog, gog::{FilterParam::*, *}};
use indicatif::{ProgressBar, ProgressStyle};
use std::env::current_dir;
use std::fs;
use std::fs::*;
use std::io;
use std::io::{SeekFrom::*, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use structopt::StructOpt;
use walkdir::WalkDir;
use {download::*, install::*, update::*};
fn main() -> Result<(), anyhow::Error> {
    #[cfg(not(debug_assertions))]
    setup_panic!();
    let mut config: Config = confy::load("wyvern", "wyvern")?;
    let args = Wyvern::from_args();
    args.verbose
        .setup_env_logger("wyvern")
        .expect("Couldn't set up logger");
    match args.command {
        Login {
            code,
            username,
            password,
        } => {
            let mut config: Config = confy::load("wyvern", "wyvern")?;
            if let Some(code) = code {
                if let Ok(token) = Token::from_login_code(code) {
                    config.token = Some(token);
                } else {
                    error!("Could not login with code");
                }
            } else if let Some(username) = username {
                let password = password.unwrap_or_else(|| {
                    let pword: String = Password::new()
                        .with_prompt("Password")
                        .interact()
                        .unwrap();
                    return pword;
                });
                let token = Token::login(
                    username,
                    password,
                    Some(|| {
                        println!("A two factor authentication code is required. Please check your email for one and enter it here.");
                        let mut token: String;
                        loop {
                            token = Input::new().with_prompt("2FA Code:").interact().unwrap();
                            token = token.trim().to_string();
                            if token.len() == 4 && token.parse::<i64>().is_ok() {
                                break;
                            }
                        }
                        token
                    }),
                );
                if token.is_err() {
                    error!("Could not login to GOG.");
                    let err = token.err().unwrap();
                    match err.kind() {
                    IncorrectCredentials => error!("Wrong email or password. Sometimes this fails because GOG's login form can be inconsistent, so you may just need to try again."),
                    NotAvailable => error!("You've triggered the captcha. 5 tries every 24 hours is allowed before the captcha is triggered. You should probably use the alternate login method or wait 24 hours"),
                    _ => error!("Error: {:?}", err),
                };
                } else {
                    config.token = Some(token.unwrap());
                }
            } else {
                config.token = Some(login());
            }
            confy::store("wyvern", "wyvern", config)?;
            ::std::process::exit(0);
        }
        _ => {}
    }
    if config.token.is_none() {
        let token = login();
        config.token = Some(token);
    }
    let token_try = config.token.unwrap().refresh();
    if token_try.is_err() {
        error!("Could not refresh token. You may need to log in again.");
        let token = login();
        config.token = Some(token);
    } else {
        config.token = Some(token_try.unwrap());
    }
    let gog = Gog::new(config.token.clone().unwrap());
    let mut sync_saves = config.sync_saves.clone();
    if sync_saves.is_some() {
        sync_saves = Some(
            sync_saves
                .unwrap()
                .replace("~", dirs::home_dir().unwrap().to_str().unwrap()),
        );
    }
    confy::store("wyvern", "wyvern", config)?;
    parse_args(args, gog, sync_saves)?;
    Ok(())
}
fn parse_args(
    args: Wyvern,
    mut gog: Gog,
    sync_saves: Option<String>,
) -> Result<Gog, ::std::io::Error> {
    match args.command {
        List { id, json } => {
            let mut games = GamesList { games: vec![] };
            if let Some(id) = id {
                let details = gog.get_game_details(id).unwrap();
                games.games.push(Game::GameInfo(details, id));
            } else {
                games.games = gog
                    .get_all_filtered_products(FilterParams::from_one(MediaType(1)))
                    .expect("Couldn't fetch games")
                    .into_iter()
                    .map(|x| Game::ProductInfo(x))
                    .collect();
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&games).expect("Couldn't deserialize games list")
                );
            } else {
                println!("Title - GameID");
                games
                    .games
                    .sort_by(|a, b| a.title().partial_cmp(&b.title()).unwrap());
                for game in games.games {
                    print!("{} - ", game.title());
                    match game {
                        Game::GameInfo(_details, id) => println!("{}", id),
                        Game::ProductInfo(pinfo) => println!("{}", pinfo.id),
                    }
                }
            }
        }
        Download {
            mut options,
            mut shortcuts,
        } => {
            if shortcuts.shortcuts {
                shortcuts.desktop = true;
                shortcuts.menu = true;
            }
            options.original = !options.original;
            if let Some(search) = options.search.clone() {
                info!("Searching for games");
                let search_results =
                    gog.get_filtered_products(FilterParams::from_one(Search(search)));
                if search_results.is_ok() {
                    info!("Game search results OK");
                    let e = search_results.unwrap().products;
                    if !options.first {
                        if e.len() > 0 {
                            let mut items: Vec<String> = vec![];
                            for pd in e.iter() {
                                items.push(format!("{} - {}", pd.title, pd.id));
                            }

                            let select = Select::new();
                            let selection = select
                                .with_prompt("Select a game to download:")
                                .default(0)
                                .items(&items)
                                .interact().expect("Couldn't pick game");

                            info!("Fetching game details");
                            let details = gog.get_game_details(e[selection].id).unwrap();
                            let pname = details.title.clone();
                            info!("Beginning download process");
                            let (name, downloaded_windows) =
                                download_prep(&gog, details, &options).unwrap();
                            if options.install_after.is_some() {
                                println!("Installing game");
                                info!("Installing game");
                                install_all(
                                    name,
                                    options.install_after.unwrap(),
                                    pname,
                                    &shortcuts,
                                    downloaded_windows,
                                    options.external_zip,
                                );
                            }
                        } else {
                            error!("Found no games when searching.");
                            std::process::exit(64);
                        }
                    } else {
                        info!("Downloading first game from results");
                        let details = gog.get_game_details(e[0].id).unwrap();
                        let pname = details.title.clone();
                        info!("Beginning download process");
                        let (name, downloaded_windows) =
                            download_prep(&gog, details, &options).unwrap();
                        if options.install_after.is_some() {
                            println!("Installing game");
                            info!("Installing game");
                            install_all(
                                name,
                                options.install_after.unwrap(),
                                pname,
                                &shortcuts,
                                downloaded_windows,
                                options.external_zip,
                            );
                        }
                    }
                } else {
                    error!("Could not find any games.");
                }
            } else if let Some(id) = options.id {
                let details = gog.get_game_details(id).unwrap();
                let pname = details.title.clone();
                info!("Beginning download process");
                let (name, downloaded_windows) = download_prep(&gog, details, &options).unwrap();
                if options.install_after.is_some() {
                    println!("Installing game");
                    info!("Installing game");
                    install_all(
                        name,
                        options.install_after.unwrap(),
                        pname,
                        &shortcuts,
                        downloaded_windows,
                        options.external_zip,
                    );
                }
            } else if options.all {
                println!("Downloading all games in library");
                let games = gog.get_games().unwrap();
                for game in games {
                    let details = gog.get_game_details(game).unwrap();
                    info!("Beginning download process");
                    download_prep(&gog, details, &options).unwrap();
                }
                if options.install_after.is_some() {
                    println!("--install does not work with --all");
                }
            } else {
                error!("Did not specify a game to download. Exiting.");
            }
        }
        Login  {..} => {}
        Extras {
            game,
            all,
            first,
            slug,
            id,
            output,
        } => {
            let details: GameDetails;
            if let Some(search) = game {
                if let Ok(results) =
                    gog.get_filtered_products(FilterParams::from_one(Search(search.clone())))
                {
                    let e = results.products;
                    if e.len() < 1 {
                        error!("Found no games named {} in your library.", search)
                    }
                    let mut i = 0;
                    if !first {
                        let selects = Select::new();
                        let select =
                            selects.with_prompt("Select a game to download extras from:");
                        for pd in e.iter() {
                            select.clone().item(format!("{} - {}", pd.title, pd.id).as_str());
                        }
                        i = select.interact().unwrap();
                    }
                    info!("Fetching game details");
                    details = gog.get_game_details(e[i].id).unwrap();
                } else {
                    error!("Could not search for games.");

                    return Ok(gog);
                }
            } else if let Some(id) = id {
                if let Ok(fetched) = gog.get_game_details(id) {
                    details = fetched;
                } else {
                    error!("Could not fetch game details. Are you sure that the id is right?");

                    return Ok(gog);
                }
            } else {
                error!("Did not specify a game.");

                return Ok(gog);
            }
            println!("Downloading extras for game {}", details.title);
            let mut folder_name = PathBuf::from(format!("{} Extras", details.title));
            if let Some(output) = output {
                folder_name = output;
            }
            if fs::metadata(&folder_name).is_err() {
                fs::create_dir(&folder_name).expect("Couldn't create extras folder");
            }
            let mut picked: Vec<usize> = vec![];
            if !all && slug.is_none() {
                let check = MultiSelect::new();
                let checks = check.with_prompt("Pick the extras you want to download");
                for ex in details.extras.iter() {
                    checks.clone().item(&ex.name);
                }
                picked = checks.interact().unwrap();
            }
            if let Some(slug) = slug {
                details.extras.iter().enumerate().for_each(|(i, x)| {
                    if x.name.trim() == slug.trim() {
                        picked.push(i);
                    }
                });
                if picked.len() == 0 {
                    error!("Couldn't find an extra named {}", slug);
                    return Ok(gog);
                }
            }

            let extra_responses: Vec<anyhow::Result<reqwest::blocking::Response, anyhow::Error>> = details
                .extras
                .iter()
                .enumerate()
                .filter(|(i, _x)| {
                    if !all {
                        return picked.contains(i);
                    } else {
                        return true;
                    }
                })
                .map(|(_i, x)| {
                    info!("Finding URL");
                    let mut url = "https://gog.com".to_string() + &x.manual_url;
                    let mut response;
                    loop {
                        let temp_response = gog.client_noredirect.borrow().get(&url).send();
                        if temp_response.is_ok() {
                            response = temp_response.unwrap();
                            let headers = response.headers();
                            // GOG appears to be inconsistent with returning either 301/302, so this just checks for a redirect location.
                            if headers.contains_key("location") {
                                url = headers
                                    .get("location")
                                    .unwrap()
                                    .to_str()
                                    .unwrap()
                                    .to_string();
                            } else {
                                break;
                            }
                        } else {
                            return Err(temp_response.err().unwrap().into());
                        }
                    }
                    Ok(response)
                })
                .collect();
            for extra in extra_responses.into_iter() {
                let extra = extra.expect("Couldn't fetch extra");
                let real_response = gog
                    .client_noredirect
                    .borrow()
                    .get(extra.url().clone())
                    .send()
                    .expect("Couldn't fetch extra data");
                let name = extra
                    .url()
                    .path_segments()
                    .unwrap()
                    .last()
                    .unwrap()
                    .to_string();
                let n_path = folder_name.join(&name);
                if fs::metadata(&n_path).is_ok() {
                    warn!("This extra has already been downloaded. Skipping.");
                    continue;
                }
                println!("Starting download of {}", name);
                let pb = ProgressBar::new(extra.content_length().unwrap());
                pb.set_style(ProgressStyle::default_bar()
                                         .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})").unwrap()
                                         .progress_chars("#>-"));
                let mut pb_read = pb.wrap_read(real_response);
                let mut file = File::create(n_path).expect("Couldn't create file");
                io::copy(&mut pb_read, &mut file).expect("Couldn't copy to target file");
                pb.finish();
            }
        }
        Interactive => {
            gog = interactive::interactive(gog, sync_saves);
        }
        Install {
            installer_name,
            path,
            mut shortcuts,
            windows,
            external_zip,
        } => {
            if shortcuts.shortcuts {
                shortcuts.desktop = true;
                shortcuts.menu = true;
            }
            info!("Starting installation");
            install(
                installer_name.clone(),
                path,
                installer_name,
                &shortcuts,
                windows,
                external_zip,
            );
        }
        #[cfg(feature = "eidolonint")]
        UpdateEidolon { force, delta } => {
            use libeidolon::games::*;
            let eidolon_games = get_games();
            for game in eidolon_games {
                if let Ok(read) = read_game(game.as_str()) {
                    if read.typeg == GameType::WyvernGOG {
                        println!("Attempting to update {}", read.pname);
                        let path = PathBuf::from(read.command);
                        let ginfo_path = path.clone().join("gameinfo");
                        update(&gog, path, ginfo_path, force, delta);
                    }
                } else {
                    println!("Could not check {}", game);
                }
            }
        }
        Sync(..) => {
            gog = sync::parse_args(gog, sync_saves, args);
        }
        Connect { .. } => {
            gog = connect::parse_args(gog, args);
        }
        Update { mut path, dlc } => {
            if path.is_none() {
                info!("Path not specified. Using current dir");
                path = Some(PathBuf::from(".".to_string()));
            }
            let path = path.unwrap();
            let game_info_path = PathBuf::from(path.join("gameinfo"));
            info!("Updating game");
            update(&gog, path, game_info_path, dlc);
        }
    };
    Ok(gog)
}
pub fn login() -> Token {
    let choices = ["OAuth Token Login", "Username/Password Login"];
    println!("It appears that you have not logged into GOG. Please pick a login method.");
    let pick = Select::new()
        .with_prompt("Login Method")
        .items(&choices)
        .interact()
        .expect("Couldn't pick a login method");
    match pick {
        //OAuth Token
        0 => {
            println!("Please go to the following URL, log into GOG, and paste the code from the resulting url's ?code parameter into the input here.");
            println!("https://login.gog.com/auth?client_id=46899977096215655&layout=client2%22&redirect_uri=https%3A%2F%2Fembed.gog.com%2Fon_login_success%3Forigin%3Dclient&response_type=code");
            io::stdout().flush().unwrap();
            let token: Token;
            loop {
                let code: String = Input::new().with_prompt("Code:").interact().unwrap();
                info!("Creating token from input");
                let attempt_token = Token::from_login_code(code.as_str());
                if attempt_token.is_ok() {
                    token = attempt_token.unwrap();
                    println!("Got token. Thanks!");
                    break;
                } else {
                    println!("Invalid code. Try again!");
                }
            }
            return token;
        }
        1 => {
            println!("Please input your credentials.");
            let username: String = Input::new()
                .with_prompt("Email")
                .interact()
                .expect("Couldn't fetch username");
            let password: String = Password::new()
                .with_prompt("Password")
                .interact()
                .expect("Couldn't fetch password");
            let token = Token::login(
                username,
                password,
                Some(|| {
                    println!("A two factor authentication code is required. Please check your email for one and enter it here.");
                    let mut token: String;
                    loop {
                        token = Input::new().with_prompt("2FA Code:").interact().unwrap();
                        token = token.trim().to_string();
                        if token.len() == 4 && token.parse::<i64>().is_ok() {
                            break;
                        }
                    }
                    token
                }),
            );
            if token.is_err() {
                error!("Could not login to GOG.");
                let err = token.err().unwrap();
                match err.kind() {
                    IncorrectCredentials => error!("Wrong email or password. Sometimes this fails because GOG's login form can be inconsistent, so you may just need to try again."),
                    NotAvailable => error!("You've triggered the captcha. 5 tries every 24 hours is allowed before the captcha is triggered. You should probably use the alternate login method or wait 24 hours"),
                    _ => error!("Error: {:?}", err),
                };
            } else {
                return token.unwrap();
            }
        }
        _ => {
            panic!("Please tell somebody about this.");
        }
    };
    std::process::exit(64);
}
fn shortcuts(name: &String, path: &std::path::Path, shortcut_opts: &ShortcutOptions) {
    if shortcut_opts.menu || shortcut_opts.desktop {
        info!("Creating shortcuts");
        let game_path = current_dir().unwrap().join(&path);
        info!("Creating text of shortcut");
        let shortcut = desktop_shortcut(name.as_str(), &game_path);
        if shortcut_opts.menu {
            info!("Adding menu shortcut");
            let desktop_path = dirs::home_dir().unwrap().join(format!(
                ".local/share/applications/gog_com-{}_1.desktop",
                name
            ));
            info!("Created menu file");
            let fd = File::create(&desktop_path);
            if fd.is_ok() {
                info!("Writing to file");
                fd.unwrap()
                    .write(shortcut.as_str().as_bytes())
                    .expect("Couldn't write to menu shortcut");
            } else {
                error!(
                    "Could not create menu shortcut. Error: {}",
                    fd.err().unwrap()
                );
            }
        }
        if shortcut_opts.desktop {
            info!("Adding desktop shortcut");
            let desktop_path = dirs::home_dir()
                .unwrap()
                .join(format!("Desktop/gog_com-{}_1.desktop", name));
            let fd = File::create(&desktop_path);
            if fd.is_ok() {
                info!("Writing to file.");
                let mut fd = fd.unwrap();
                fd.write(shortcut.as_str().as_bytes())
                    .expect("Couldn't write to desktop shortcut");
                info!("Setting permissions");
                fd.set_permissions(Permissions::from_mode(0o0774))
                    .expect("Couldn't make desktop shortcut executable");
            } else {
                error!(
                    "Could not create desktop shortcut. Error: {}",
                    fd.err().unwrap()
                );
            }
        }
    }
}
fn desktop_shortcut(name: impl Into<String>, path: &std::path::Path) -> String {
    let name = name.into();
    let path = current_dir().unwrap().join(path);
    format!("[Desktop Entry]\nEncoding=UTF-8\nValue=1.0\nType=Application\nName={}\nGenericName={}\nComment={}\nIcon={}\nExec=\"{}\" \"\"\nCategories=Game;\nPath={}",name,name,name,path.join("support/icon.png").to_str().unwrap(),path.join("start.sh").to_str().unwrap(), path.to_str().unwrap())
}
