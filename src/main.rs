#![feature(lazy_cell)]
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::thread;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::path::Path;

mod bundle;
use bundle::BundleFd;
mod file;
use file::ExtractOptions;
use file::Pool;
mod hash;
use hash::MurmurHash;
mod oodle;
mod read;
use read::ChunkReader;
mod scoped_fs;
use scoped_fs::ScopedFs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dictionary = fs::read_to_string("dictionary.txt");
    let (dictionary, skip_unknown) = if let Ok(data) = dictionary.as_ref() {
        let mut dict = HashMap::with_capacity(0x1000);
        for key in data.lines() {
            if !key.is_empty() {
                dict.insert(MurmurHash::new(key), key);
            }
        }
        (dict, true)
    } else {
        (HashMap::new(), false)
    };

    let mut args = std::env::args_os();
    args.next();
    let arg = args.next();

    let darktide_path = steam_find::get_steam_app(1361210).map(|app| app.path);
    let bundle_path;
    let path = if arg.as_ref().filter(|p| p == &"-").is_some() {
        match darktide_path {
            Ok(ref path) => {
                bundle_path = path.join("bundle");
                Some(bundle_path.as_ref())
            }
            Err(e) => {
                eprintln!("Darktide directory could not be found automatically");
                eprintln!();
                return Err(Box::new(e));
            }
        }
    } else {
        arg.as_ref().map(Path::new)
    };

    if let Some(path) = path {
        let oodle = match oodle::Oodle::load("oo2core_8_win64.dll") {
            Ok(oodle) => oodle,
            Err(e) => {
                if let Some(oodle) = path.parent().map(|p| p.join("binaries/oo2core_8_win64.dll"))
                    .and_then(|p| oodle::Oodle::load(p).ok())
                    .or_else(|| darktide_path.ok().map(|path| path.join("binaries/oo2core_8_win64.dll"))
                        .and_then(|p| oodle::Oodle::load(p).ok()))
                {
                    oodle
                } else {
                    eprintln!("oo2core_8_win64.dll could not be loaded");
                    eprintln!("copy the dll from the Darktide binaries folder next to limn");
                    eprintln!();
                    return Err(Box::new(e));
                }
            }
        };

        let options = ExtractOptions {
            target: path,
            out: ScopedFs::new(Path::new("./out")),
            oodle: &oodle,
            dictionary: &dictionary,
            dictionary_short: &dictionary.iter().map(|(k, v)| (k.clone_short(), *v)).collect(),
            skip_unknown,
            as_blob: false,
        };

        let filter = args.next().as_ref()
            .and_then(|a| a.to_str())
            .map(|s| hash::murmurhash64(s.as_bytes()));

        let start = std::time::Instant::now();
        let num_files = if let Ok(read_dir) = fs::read_dir(path) {
            let read_dir = read_dir.collect::<Vec<_>>();
            let num_files = AtomicU32::new(0);
            let count = AtomicUsize::new(0);
            let file_i = AtomicUsize::new(0);

            let duplicates = Mutex::new(HashMap::with_capacity(0x10000));
            let num_threads = thread::available_parallelism()
                .map(|i| i.get())
                .unwrap_or(0)
                .saturating_sub(1)
                .max(1);
            thread::scope(|s| {
                for _ in 0..num_threads {
                    s.spawn(|| {
                        let mut pool = Pool::new();
                        let mut buffer_reader = vec![0_u8; 0x80000];
                        let mut bundle_buf = Vec::new();

                        while let Some(fd) = read_dir.get(file_i.fetch_add(1, Ordering::SeqCst)) {
                            let fd = fd.as_ref().unwrap();
                            let meta = fd.metadata().unwrap();
                            let path = fd.path();
                            let bundle_hash = bundle_hash_from(&path);
                            if meta.is_file() && bundle_hash.is_some() && path.extension().is_none() {
                                let bundle = File::open(&path).unwrap();
                                let mut rdr = ChunkReader::new(&mut buffer_reader, bundle);
                                let num = extract_bundle(
                                    &mut pool,
                                    &mut rdr,
                                    &mut bundle_buf,
                                    bundle_hash,
                                    Some(&duplicates),
                                    &options,
                                    filter,
                                ).unwrap();
                                num_files.fetch_add(num, Ordering::SeqCst);
                                let count = count.fetch_add(1, Ordering::SeqCst);
                                if count > 0 && count % 10 == 0 {
                                    println!("{count}");
                                }
                            }
                        }
                    });
                }
            });

            let count = count.into_inner();
            if count % 10 != 0 {
                println!("{count}");
            }

            num_files.into_inner()
        } else if let Ok(bundle) = File::open(path) {
            let bundle_hash = bundle_hash_from(path);
            let mut buf = vec![0; 0x80000];
            let mut rdr = ChunkReader::new(&mut buf, bundle);
            extract_bundle(
                &mut Pool::new(),
                &mut rdr,
                &mut Vec::new(),
                bundle_hash,
                None,
                &options,
                filter,
            )?
        } else {
            panic!("PATH argument was invalid");
        };
        println!();
        println!("DONE");
        println!("took {}ms", start.elapsed().as_millis());
        println!("extracted {num_files} files");
    } else {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        println!("{}", env!("CARGO_PKG_REPOSITORY"));
        println!();
        println!("limn extracts files from resource bundles used in Darktide.");
        println!();
        println!("limn uses oo2core_8_win64.dll to decompress the bundle. If it fails to load");
        println!("oo2core_8_win64.dll then copy it from the Darktide binaries folder next to limn.");
        println!();
        println!("USAGE:");
        println!("limn.exe <PATH> <FILTER>");
        println!();
        println!("ARGS:");
        println!("    <PATH>    A bundle or directory of bundles to extract.");
        println!("    <FILTER>  If present only extract files with this extension.");
    }

    Ok(())
}

fn extract_bundle(
    pool: &mut Pool,
    mut rdr: impl Read + Seek,
    bundle_buf: &mut Vec<u8>,
    bundle_hash: Option<u64>,
    duplicates: Option<&Mutex<HashMap<(u64, u64), u64>>>,
    options: &ExtractOptions<'_>,
    filter: Option<u64>,
) -> io::Result<u32> {
    bundle_buf.clear();
    let mut bundle = BundleFd::new(bundle_hash, &mut rdr)?;
    let mut num_targets = if let Some(filter_ext) = filter {
        let mut count = 0;
        for file in bundle.index() {
            if file.ext == filter_ext {
                if options.skip_unknown
                    && !options.dictionary.contains_key(&MurmurHash::from(file.name))
                {
                    continue;
                }
                count += 1;
            }
        }

        if count == 0 {
            return Ok(0);
        } else {
            Some(count)
        }
    } else {
        None
    };

    let mut count = 0;
    let mut files = bundle.files(options.oodle, bundle_buf);
    while let Ok(Some(file)) = files.next_file().map_err(|e| panic!("{:016x} - {}", bundle_hash.unwrap_or(0), e)) {
        if options.skip_unknown
            && file.ext != /*lua*/0xa14e8dfa2cd117e2
            && !(filter == Some(file.ext) && file.ext == /*strings*/0x0d972bab10b40fd3)
            && !options.dictionary.contains_key(&MurmurHash::from(file.name))
        {
            continue;
        }

        if let Some(filter_ext) = filter {
            let num_targets = num_targets.as_mut().unwrap();
            if *num_targets == 0 {
                break;
            }

            if file.ext != filter_ext {
                continue;
            } else {
                *num_targets -= 1;
            }
        }

        if let Some(duplicates) = duplicates {
            let key = (file.name, file.ext);
            let mut duplicates = duplicates.lock().unwrap();
            if let Some(num_dupes) = duplicates.get_mut(&key) {
                *num_dupes += 1;
                continue;
            } else {
                duplicates.insert(key, 1);
            }
        }

        match file::extract(file, pool, options) {
            Ok(_wrote) => count += 1,
            Err(_e) => (),//eprintln!("{e}"),
        }
    }

    Ok(count)
}

fn bundle_hash_from(path: &Path) -> Option<u64> {
    let name = path.file_stem()?;
    u64::from_str_radix(name.to_str()?, 16).ok()
}
