/* Make DMFS (MkDMFS)
 * 
 * Create a DMFS image file to embed in a diosix hypervisor
 * 
 * usage: cargo run -- [--verbose] -m <manifest toml file> -t <target architecture> -q <quality> -o <outfile>
 * 
 * Options:
 * <manifest toml file>  = pathname of manifest configuration file. if unspecified, it'll search up the tree for manifest.toml
 * <target architecture> = architecture prefix the hypervisor will run on. eg: riscv64gc-unknown-none-elf
 * <quality>             = 'debug' to use the debug-enabled build of components, or 'release' for the release-grade builds
 * <outfile>             = pathname of the generated dmfs image file
 * --verbose             = output progress of the build
 * --skip-downloads      = don't download any guest OSes
 * --skip-buildroot      = don't build any guest OSes from source
 * --skip-services       = don't include any system services at all
 * --skip-guests         = don't include guest OSes at all
 * 
 * mkdmfs takes its settings from the command line, and if any are omitted, it falls back
 * to its TOML-compliant manifest configuration file. If the location of this file isn't specified on the command line,
 * MkDMFS searches up the host ile system tree from the current working directory for a file called manifest.toml.
 * If no configuration file is found or supplied, MkDMFS will exit with an error. The file format is:
 * 
 * defaults.arch = architecture to use if <target architecture> is unspecified
 * defaults.quality = build quality to use if <quality> is unspecified
 * defaults.outfile = pathname of generated image if <outfile> is unspecified
 * defaults.ram = number of megabytes of RAM to assign to a capsule if unspecified
 * defaults.cpus = number of virtual CPU cores to assign to a capsule if unspecified
 * banners.path = pathname of the directory containing the arch-specific boot banners. <base target architecture>.txt will be included, if present
 * banners.welcome = pathname of the generic boot banner text file to be included
 * services.include = array of services to include in the dmfs image from the services directory
 * service.<name>.path = location of the service's source code directory (required)
 * service.<name>.description = description of what this service does (required)
 * service.<name>.properties = array of permissions and other properties granted to this service
 * service.<name>.ram = 
 * service.<name>.cpus = 
 * guest.<label>.path = host file system directory containing guest kernel image <label> (required)
 * guest.<label>.url = URL from which to fetch the guest kernel image if it's not present
 * guest.<label>.description = brief description of this guest (required)
 * guest.<label>.ram = number of megabytes of RAM to allocate for this guest
 * guest.<label>.cpus = number of virtual CPU cores to allocate for this guest
 * target.<target architecture>.guests = array of <label>s for guests to include in the image for the target arch
 * 
 * Recognized properties:
 * auto_crash_restart = restart if crashed
 * service_console = allow it to register as console service
 * console_write = allow it to write direct to the console
 * console_read = allow it to read direct from the console
 * 
 * The pathnames are relative to <manifest toml file> or the found manifest.toml
 * Base target architecture = riscv, aarch64, powerpc, etc.
 * 
 * (c) Chris Williams, 2020.
 *
 * See LICENSE for usage and copying.
 */

extern crate dmfs;
extern crate clap;
extern crate toml;
extern crate serde;
extern crate serde_derive;

use std::env;
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::fs::{read_to_string, create_dir_all, File};
use std::collections::HashMap;

extern crate reqwest;

extern crate regex;
use regex::Regex;

use clap::{*, App};
use serde_derive::Deserialize;

use dmfs::{Manifest, ManifestObject, ManifestObjectType, ManifestObjectData};

/* define the manifest configutation TOML file */
#[derive(Deserialize)]
struct Config
{
    defaults: Defaults,
    banners: Option<Banners>,
    services: Option<Services>,
    service: Option<HashMap<String, Service>>, 
    guest: Option<HashMap<String, Guest>>,
    target: Option<HashMap<String, Target>>
}

#[derive(Deserialize)]
struct Defaults
{
    arch: Option<String>,
    quality: Option<String>,
    outfile: Option<String>
}

#[derive(Deserialize)]
struct Banners
{
    path: Option<String>,
    welcome: Option<String>
}

#[derive(Deserialize)]
struct Services
{
    include: Option<Vec<String>>
}

#[derive(Deserialize)]
struct Service
{
    path: String,
    description: String,
    properties: Option<Vec<String>>
}

#[derive(Deserialize)]
struct Guest
{
    path: String,
    url: Option<String>,
    description: String
}

#[derive(Deserialize)]
struct Target
{
    guests: Option<Vec<String>>
}

/* default manifest file name */
static MANIFEST_FILE: &str = "manifest.toml";

/* max attempts to search the host file system for a config file */
static SEARCH_MAX: usize = 100;

/* these could be fancy enums and whatnot but we're dealing primarily in strings in this program,
so it seems an unnecessary faff at the moment to decode and re-encode them. we'll leave them as strings */
struct Settings
{
    /* pathname of the manifest configuration file's parent directory */
    config_dir: PathBuf,

    /* set by the command line, or from the configuration's file defaults, or None if unspecified */
    output_filename: Option<String>,
    target_arch: Option<String>,
    quality: Option<String>,
    verbose: bool,
    no_downloads: bool,
    no_services: bool,
    no_guests: bool,
    
    /* set by the manifest configuration file */
    config: Config
}

impl Settings
{
    pub fn new() -> Settings
    {
        /* decode the command-line options. this call will also bail out
        with a message to the user if the invocation syntax is incorrect */
        let opts = App::new("dmfs")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Create DMFS images from a collection of files")
        .args_from_usage("
            -m, --manifest=[FILE] 'Sets location of manifest config file'
            -t, --target=[ARCH]   'Sets architecture of target system'
            -q, --quality=[LEVEL] 'Set whether this is a debug or release build'
            -o, --output=[FILE]   'Set location of generated image file'
            -v, --verbose         'Output progress of image creation'
            --skip-downloads      'Don't download guest OS images'
            --skip-buildroot      'Don't build guest OSes using buildroot'
            --skip-services       'Don't include system services'
            --skip-guests         'Don't include guest OSes'")
        .get_matches();

        /* try to find the toml configuration file: first from the command line, and next by searching up through the tree */
        let config_location = match opts.value_of("manifest")
        {
            Some(v) =>
            {
                let mut pb = PathBuf::new();
                pb.push(v);
                pb
            },
            None => match search_for_config(MANIFEST_FILE)
            {
                Some(p) => p,
                None => fatal_error(format!("Can't find manifest configuration file {:?} in host file system", MANIFEST_FILE))
            }
        };

        /* read in the contents of the configuration file */
        let config_contents = match read_to_string(&config_location)
        {
            Ok(c) => c,
            Err(e) => fatal_error(format!("Can't read manifest configuration file {:?} in host file system: {}", config_location, e))
        };

        /* and finally, parse it */
        let config: Config = match toml::from_str(config_contents.as_str())
        {
            Ok(c) => c,
            Err(e) => fatal_error(format!("Can't parse manifest configutation file {:?}: {}", config_location, e))
        };

        /* get the settings from the command line, or fall back to defaults in the manifest config file, if any */
        let output_filename = match opts.value_of("output")
        {
            Some(of) => Some(String::from(of)),
            None => match config.defaults.outfile
            {
                Some(ref s) => Some(s.clone()),
                None => None
            }
        };
        let target_arch = match opts.value_of("target")
        {
            Some(ta) => Some(String::from(ta)),
            None => match config.defaults.arch 
            {
                Some(ref s) => Some(s.clone()),
                None => None
            }
        };
        let quality = match opts.value_of("quality")
        {
            Some(q) => Some(String::from(q)),
            None => match config.defaults.quality
            {
                Some(ref s) => Some(s.clone()),
                None => None
            }
        };

        /* these aren't defined in the toml, only at the command line */
        let verbose = opts.is_present("verbose");
        let no_downloads = opts.is_present("skip-downloads");
        let no_services  = opts.is_present("skip-services");
        let no_guests    = opts.is_present("skip-guests");

        /* generate a structure to hold all the settings together */
        Settings
        {
            /* save the directory pathname of where we read in our config */
            config_dir: match config_location.parent()
            {
                Some(p) => p.to_path_buf(),
                None => fatal_error(format!("Can't get directory of manifest configuration file"))
            },

            /* stash our parsed toml config file */
            config,

            /* stash settings, either from the command line or the config file, or None for not specified */
            verbose,
            no_downloads,
            no_services,
            no_guests,
            output_filename,
            target_arch,
            quality
        }
    }
}

/* asynchronous wrapping needed for reqwest'ing files from the network/internet */
#[tokio::main]
async fn main() -> Result<()> 
{
    /* get our instructions from the command line. this function call
    will bail out if there's a problem with the cmd line arguments */
    let settings = Settings::new();

    /* create an empty manifest object that describes the dmfs we want to generate */
    let mut manifest = Manifest::new();

    /* make sure all paths are based from the config file's directory */
    let mut base = PathBuf::new();
    base.push(settings.config_dir);

    /* banners are optional, so none defined, don't worry */
    if let Some(banners) = settings.config.banners
    {
        /* start with an architecture-specific banner, if possible */
        if let Some(banner_dir) = banners.path
        {
            if let Some(target_arch) = &settings.target_arch
            {
                if let Some(base_arch) = get_base_arch(&target_arch)
                {
                    let mut p = base.clone();
                    p.push(&banner_dir);
                    p.push(format!("{}.txt", base_arch));
                    manifest.add(ManifestObject::new
                    (
                        ManifestObjectType::BootMsg,
                        Path::new(&p).file_name().unwrap().to_str().unwrap().to_string(),
                        format!("Boot banner text for {} systems", base_arch),
                        ManifestObjectData::Bytes(load_file(&p, settings.verbose)),
                        None
                    ));
                }
            }
        }

        /* next the generic welcome banner text, if defined */
        if let Some(welcome) = banners.welcome
        {
            let mut p = base.clone();
            p.push(&welcome);
            manifest.add(ManifestObject::new
            (
                ManifestObjectType::BootMsg,
                Path::new(&welcome).file_name().unwrap().to_str().unwrap().to_string(),
                format!("Main boot banner text"),
                ManifestObjectData::Bytes(load_file(&p, settings.verbose)),
                None
            ));
        }
    }

    /* include the system services, if any are defined and if allowed */
    if let (Some(services), false) = (settings.config.services, settings.no_services)
    {
        /* get the hashtable of defined available services */
        if let Some(available_services) = settings.config.service
        {
            /* get the list of services to include */
            if let Some(services_to_include) = services.include
            {
                /* run through that list */
                for service_name in services_to_include
                {
                    /* look up the service from its name */
                    if let Some(service) = available_services.get(&service_name)
                    {
                        /* drill down to the service's binary we want to include */
                        let mut p = base.clone();
                        p.push(&service.path);
                        p.push("target");
                        
                        /* skip the arch directory if it doesn't exist -- may mean we're self-hosting */
                        match &settings.target_arch
                        {
                            Some(ta) =>
                            {
                                let mut test = p.clone();
                                test.push(ta);
                                if test.as_path().exists() == true
                                {
                                    p.push(&ta);
                                }
                            },
                            None => ()
                        }

                        /* select the appropriate debug or release build */
                        if let Some(q) = &settings.quality
                        {
                            p.push(q);
                            p.push(&service_name);
                        }

                        manifest.add(ManifestObject::new
                        (
                            ManifestObjectType::SystemService,
                            (&service_name).to_string(),
                            service.description.clone(),
                            ManifestObjectData::Bytes(load_file(&p, settings.verbose)),
                            service.properties.clone()
                        ));
                    }
                }
            }
        }
    }

    /* get the architecture we're generating a dmfs image for */
    if let (Some(target_arch), false) = (&settings.target_arch, settings.no_guests)
    {
        /* get a list of supported build targets */
        if let Some(possible_targets) = settings.config.target
        {
            /* does the target architecture have an entry in the supported targets list? */
            if let Some(target_entry) = possible_targets.get(&target_arch.clone())
            {
                /* if so, get the target sarchitecture's list of guests to include */
                if let Some(targets_guests) = &target_entry.guests
                {
                    /* fetch the list of available guests */
                    let available_guests = match settings.config.guest
                    {
                        Some(hashtbl) => hashtbl,
                        None => HashMap::new()
                    };

                    /* and include the ones required by this target */
                    for guest in targets_guests
                    {
                        match available_guests.get(&guest.clone())
                        {
                            Some(g) =>
                            {
                                /* generate path name of guest image */
                                let mut path = base.clone();
                                path.push(&g.path);
                                /* make sure a directory is present to house the guest */
                                if let Err(e) = create_dir_all(&path)
                                {
                                    fatal_error(format!("Can't ensure directory {} exists for guest {} ({})",
                                        &path.to_str().unwrap(), &guest, e));
                                }
                                path.push(&guest);

                                /* if it doesn't exist, try fetching from its URL */
                                if Path::new(&path).exists() == false
                                {
                                    if let (Some(url), false) = (&g.url, settings.no_downloads)
                                    {
                                        if settings.verbose == true
                                        {
                                            println!("Downloading guest OS {}...", &g.description);
                                        }

                                        /* fetch the guest */
                                        let data = match reqwest::get(url).await
                                        {
                                            Ok(response) => response.bytes().await,
                                            Err(e) => fatal_error(format!("Can't fetch {} for {}: {}",
                                                        &url, &guest, e))
                                        };

                                        /* and write it to storage */
                                        let mut fh = match File::create(&path)
                                        {
                                            Ok(fh) => fh,
                                            Err(e) => fatal_error(format!("Can't create {} for {}: {}",
                                                                    &path.to_str().unwrap(), &guest, e))
                                        };

                                        let mut slice: &[u8] = data.as_ref().unwrap();

                                        if let Err(e) = io::copy(&mut slice, &mut fh)
                                        {
                                            fatal_error(format!("Failed to write {} for {}: {}",
                                                &path.to_str().unwrap(), &guest, e));
                                        }
                                    }
                                    else
                                    {
                                        /* the load_file() will fail anyway but why not handle it here */
                                        fatal_error(format!("Can't find guest OS file {}", path.to_str().unwrap()));
                                    }
                                }

                                if settings.verbose == true
                                {
                                    println!("Including guest OS {}...", &g.description);
                                }

                                manifest.add(ManifestObject::new(
                                    ManifestObjectType::GuestOS,
                                    guest.clone(),
                                    g.description.clone(),
                                    ManifestObjectData::Bytes(load_file(&path, settings.verbose)),
                                    None
                                ));
                            },
                            None => fatal_error(format!("Guest {} required by target architecture {} not defined", guest, target_arch))
                        }
                    }
                }
            }
        }
    }

    /* now generate the dmfs image */
    let bytes = match manifest.to_image()
    {
        Ok(b) => b,
        Err(e) => fatal_error(format!("Failed to generate dmfs image: {:?}", e))
    };

    /* generate filename of our dmfs image */
    let mut of = base.clone();
    of.push(match settings.output_filename
    {
        Some(f) => f,
        None => fatal_error(format!("No output filename specified"))
    });

    /* create a file to write out the dmfs image */
    let mut file = match File::create(&of)
    {
        Ok(fh) => fh,
        Err(e) => fatal_error(format!("Can't create output file {:?}: {}", of, e))
    };

    /* write out the bytes */
    match file.write_all(bytes.as_slice())
    {
        Ok(()) => if settings.verbose == true
        {
            println!("{} bytes of dmfs image written successfully to {:?}", bytes.len(), of);
        },
        Err(e) => fatal_error(format!("Failed during dmfs image write to file: {}", e))
    }

    Ok(())
}

/* starting in the current working directory, check for the presence of the
   required config file, and if it's not there, check inside the parent.
   continue up the host file system tree until after hitting the root node.
   this function gives up after SEARCH_MAX iterations to avoid infinite loops.
   => leafname = config file leafname to look for
   <= returns filename of found config file, or None if unsuccessful */
fn search_for_config(leafname: &str) -> Option<PathBuf>
{
    let mut path = match env::current_dir()
    {
        Ok(p) => p,
        Err(e) => fatal_error(format!("Can't get the current working directory ({})", e))
    };

    /* avoid an infinite loop in case something weird happens.
    give up after this arbitrary number of attempts to go up
    through the build host's file system tree */
    for _ in 0..SEARCH_MAX
    {
        let mut attempt = path.clone();
        attempt.push(leafname);
        if attempt.exists() == true
        {
            return Some(attempt);
        }

        path = match path.parent()
        {
            Some(p) => p.to_path_buf(),
            None => return None /* give up if we can't go any higher in the tree */
        }
    }

    None
}

/* load a file from the host file system into memory.
bails out if it can't read the file */
fn load_file(path: &PathBuf, verbose: bool) -> Vec<u8>
{
    let mut buffer = Vec::new();

    let mut fh = match File::open(&path)
    {
        Ok(fh) => fh,
        Err(e) => fatal_error(format!("Can't open file {}: {}", path.display(), e))
    };

    match fh.read_to_end(&mut buffer)
    {
        Ok(size) => if verbose == true
        {
            println!("Read {} bytes of {}", size, path.display());
        },
        Err(e) => fatal_error(format!("Couldn't read file {}: {}", path.display(), e))
    }

    buffer
}

/* translate a full target architecture into a base architecture */
fn get_base_arch(full_target: &String) -> Option<String>
{
    let re = Regex::new(r"(?P<arch>riscv|aarch64|arm|powerpc64|x86_64){1}").unwrap();
    let matches = re.captures(&full_target);
    if matches.is_none() == true
    {
        return None; /* unknown architecture */
    }

    Some((matches.unwrap())["arch"].to_string())
}

/* bail out with an error msg */
fn fatal_error(msg: String) -> !
{
    /* ignores the verbose setting */
    eprintln!("mkdmfs error: {}", msg);
    exit(1);
}