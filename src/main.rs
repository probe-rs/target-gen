pub mod algorithm_binary;
pub mod chip;
pub mod chip_family;
pub mod error;
pub mod flash_device;
pub mod parser;

use crate::error::Error;
use chip::Chip;
use chip_family::ChipFamily;
use cmsis_pack::pdsc::Core;
use cmsis_pack::pdsc::Package;
use cmsis_pack::pdsc::Processors;
use cmsis_pack::utils::FromElem;
use probe_rs::config::flash_algorithm::RawFlashAlgorithm;
use probe_rs::config::memory::{FlashRegion, MemoryRegion, RamRegion};
use std::fs::{self, File};
use std::path::Path;

fn main() {
    let args: Vec<_> = std::env::args().collect();
    // The directory in which to look for the .pdsc file.
    let in_dir = &std::path::Path::new(&args[1]);
    let out_dir = &std::path::Path::new(&args[2]);

    let mut families = Vec::<ChipFamily>::new();

    // Look for the .pdsc file in the given dir and it's child directories.
    visit_dirs(Path::new(&in_dir), &mut |pdsc, mut archive| {
        // Forge a definition file for each device in the .pdsc file.
        for (device_name, device) in pdsc.devices.0 {
            // Check if this device family is already known.
            let mut potential_family = families
                .iter_mut()
                .find(|family| family.name == device.family);

            // Extract the RAM info from the .pdsc file.
            let mut ram = None;
            for memory in device.memories.0.values() {
                if memory.default && memory.access.read && memory.access.write {
                    ram = Some(RamRegion {
                        range: memory.start as u32..memory.start as u32 + memory.size as u32,
                        is_boot_memory: memory.startup,
                    });
                    break;
                }
            }

            // Extract the flash algorithm, block & sector size and the erased byte value from the ELF binary.
            // Only do this if this wasn't done yet for this family.
            let mut page_size = 0;
            let mut sector_size = 0;
            let mut erased_byte_value = 0xFF;
            let flash_algorithms = device
                .algorithms
                .iter()
                .map(|flash_algorithm| {
                    let (algo, ps, ss, ebv) = if let Some(ref mut archive) = archive {
                        crate::parser::extract_flash_algo(
                            archive
                                .by_name(&flash_algorithm.file_name.as_path().to_string_lossy())
                                .unwrap(),
                            flash_algorithm.file_name.as_path(),
                            flash_algorithm.default,
                        )
                    } else {
                        crate::parser::extract_flash_algo(
                            std::fs::File::open(in_dir.join(&flash_algorithm.file_name).as_path())
                                .unwrap(),
                            flash_algorithm.file_name.as_path(),
                            flash_algorithm.default,
                        )
                    }?;

                    page_size = ps;
                    sector_size = ss;
                    erased_byte_value = ebv;

                    Ok(algo)
                })
                .filter_map(|flash_algorithm: Result<RawFlashAlgorithm, Error>| {
                    match flash_algorithm {
                        Ok(flash_algorithm) => Some(flash_algorithm),
                        Err(error) => {
                            log::warn!("Failed to parse flash algorithm.");
                            log::warn!("Reason: {:?}", error);
                            None
                        }
                    }
                })
                .collect::<Vec<_>>();

            // Extract the flash info from the .pdsc file.
            let mut flash = None;
            for memory in device.memories.0.values() {
                if memory.default && memory.access.read && memory.access.execute {
                    flash = Some(FlashRegion {
                        range: memory.start as u32..memory.start as u32 + memory.size as u32,
                        is_boot_memory: memory.startup,
                        sector_size,
                        page_size,
                        erased_byte_value,
                    });
                    break;
                }
            }

            // Get the core type.
            let core = if let Processors::Symmetric(processor) = device.processor {
                match processor.core {
                    Core::CortexM0 => "M0",
                    Core::CortexM0Plus => "M0",
                    Core::CortexM4 => "M4",
                    Core::CortexM3 => "M3",
                    c => {
                        log::warn!("Core {:?} not supported yet.", c);
                        ""
                    }
                }
            } else {
                log::warn!("Asymmetric cores are not supported yet.");
                ""
            };

            let family = if let Some(ref mut family) = potential_family {
                family
            } else {
                families.push(ChipFamily::new(
                    device.family,
                    flash_algorithms,
                    core.to_owned(),
                ));
                // This unwrap is always safe as we insert at least one item previously.
                families.last_mut().unwrap()
            };

            family.variants.push(Chip {
                name: device_name,
                memory_map: vec![
                    MemoryRegion::Ram(ram.unwrap()),
                    MemoryRegion::Flash(flash.unwrap()),
                ],
            });
        }

        Ok(())
    })
    .unwrap();

    for family in &families {
        let path = out_dir.join(family.name.clone() + ".yaml");
        let file = std::fs::File::create(&path).expect(&format!("Opening {:?} failed", path));
        serde_yaml::to_writer(file, &family).unwrap();
    }
}

// one possible implementation of walking a directory only visiting files
fn visit_dirs<T>(
    path: &Path,
    cb: &mut dyn FnMut(Package, Option<&mut zip::ZipArchive<File>>) -> Result<T, Error>,
) -> Result<(), Error> {
    // If we get a dir, look for all .pdsc files.
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit_dirs(&path, cb)?;
            } else if let Some(extension) = path.as_path().extension() {
                if extension == "pdsc" {
                    cb(Package::from_path(entry.path().as_path()).unwrap(), None)?;
                }
            }
        }
    } else if let Some(extension) = path.extension() {
        if extension == "pack" {
            // If we get a file, try to unpack it.
            let file = fs::File::open(&path).unwrap();

            match zip::ZipArchive::new(file) {
                Ok(mut archive) => {
                    let pdsc =
                        find_pdsc_in_archive(&mut archive).map_or_else(String::new, |mut pdsc| {
                            let mut pdsc_string = String::new();
                            use std::io::Read;
                            pdsc.read_to_string(&mut pdsc_string).unwrap();
                            pdsc_string
                        });
                    cb(Package::from_string(&pdsc).unwrap(), Some(&mut archive))?;
                }
                Err(e) => {
                    log::error!("Zip file could not be read. Reason:");
                    log::error!("{:?}", e);
                    std::process::exit(1);
                }
            };
        }
    }
    Ok(())
}

/// Extracts the pdsc out of a ZIP archive.
fn find_pdsc_in_archive(archive: &mut zip::ZipArchive<File>) -> Option<zip::read::ZipFile> {
    let mut index = None;
    for i in 0..archive.len() {
        let file = archive.by_index(i).unwrap();
        let outpath = file.sanitized_name();

        if let Some(extension) = outpath.as_path().extension() {
            if extension == "pdsc" {
                index = Some(i);
                break;
            }
        }
    }
    if let Some(index) = index {
        Some(archive.by_index(index).unwrap())
    } else {
        None
    }
}
