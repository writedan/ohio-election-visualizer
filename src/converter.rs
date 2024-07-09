use crate::Log;
use std::rc::Rc;
use std::cell::RefCell;

#[derive(Debug)]
struct County {
    name: String,
    id: i64,
}

#[derive(Debug)]
enum MunicipalType {
    City, Township, Mixed
}

#[derive(Debug)]
struct Municipality {
    name: String,
    fips: String,
    r#type: MunicipalType,
    precincts: Rc<RefCell<Vec<Rc<Precinct>>>>,
    merges: Vec<String>, // list of fips been merged with
}

#[derive(Debug)]
struct Precinct {
    name: String,
    county: Rc<County>,
    row_id: u32
}

impl std::hash::Hash for Precinct {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.row_id.hash(state);
    }
}

impl PartialEq for Precinct {
    fn eq(&self, other: &Self) -> bool {
        self.row_id == other.row_id
    }
}

impl Eq for Precinct {}

pub fn run(election_path: String, name: &Option<String>) {
    use std::collections::{HashMap, HashSet};
    use rusqlite::Connection;
    use std::path::PathBuf;
    use colored::Colorize;
    use chrono::Datelike;
    use crate::emit;
    use std::fs::File;
    use std::io::Write;
    use calamine::{Xlsx, Reader};

    let workbook_uri: PathBuf = election_path.clone().into();
    let precinct_wb = workbook_uri.join("precinct-conversions.xlsx"); // precinct to city/township FIPS; county abbreviation to county names
    let municipal_wb = workbook_uri.join("municipal-codes.xlsx"); // fips codes to municipal names (and canonical county)
    let results_wbs = find_matching_files(&workbook_uri, "election-results");
    if !precinct_wb.exists() {
        return emit(Log::Error(format!("File does not exist: {}", precinct_wb.display().to_string().underline())));
    }

    if !municipal_wb.exists() {
        return emit(Log::Error(format!("File does not exist: {}", municipal_wb.display().to_string().underline())));
    }

    if results_wbs.len() == 0 {
        return emit(Log::Error(format!("No result workbooks found in {}", workbook_uri.display().to_string().underline())));
    }

    // first we need to identify which municipalities to exclude from the final map
    // as well as setup our tables
    let mut filter_file = match File::create("map-filter.temp") {
        Ok(file) => file,
        Err(why) => return emit(Log::Error(format!("unable to open {}: {}", "map-filter.temp".underline(), why.to_string().underline())))
    };

    // these municiplaities will be merged together
    let mut merge_file = match File::create("map-merge.temp") {
        Ok(file) => file,
        Err(why) => return emit(Log::Error(format!("unable to open {}: {}", "map-merge.temp".underline(), why.to_string().underline())))
    };

    if !PathBuf::from("elections.db").exists() {
        emit(Log::Error(format!("file does not exist: {}", "elections.db".underline())));
        return emit(Log::Info(format!("run the {} module", "init".underline())));
    }

    let conn = match Connection::open("elections.db") {
        Ok(conn) => conn,
        Err(why) => return emit(Log::Error(format!("unable to establish connection: {}", why.to_string().underline())))
    };

    let mut conn = match Connection::open("elections.db") {
        Ok(conn) => conn,
        Err(why) => return emit(Log::Error(format!("unable to establish connection: {}", why.to_string().underline())))
    };

    let mut conn = conn.savepoint().unwrap();
    emit(Log::Info("If any error occurs the database will be rolled back to this point and no further action is necessary."));

    let mut precinct_wb = calamine::open_workbook_auto(precinct_wb).unwrap();
    let mut county_wb = precinct_wb.worksheet_range("counties").unwrap();
    let mut precinct_wb = precinct_wb.worksheet_range("precincts").unwrap();
    let mut municipal_wb = calamine::open_workbook_auto(municipal_wb).unwrap().worksheet_range("Sheet1").unwrap();
    let mut results_wbs: Vec<_> = results_wbs.iter().map(|wb| {
        print!("Opening workbook {}", wb.display().to_string().underline());
        std::io::stdout().flush().expect("Unable to flush stdout.");
        let mut wb = calamine::open_workbook_auto(wb).unwrap();
        println!(" {}", "done".green());
        let mut sheets = Vec::new();
        for x in wb.sheet_names() {
            print!("Loading sheet {}", x.underline());
            std::io::stdout().flush().expect("Unable to flush stdout.");
            if let "Contents" | "Master" = x.as_str() {
                println!(" {}", "skipped".yellow());
                continue; // we pass over this one because all its data is kept in the other sheets
            }

            sheets.push(wb.worksheet_range(&x).unwrap());

            println!(" {}", "done".green());
        }

        sheets
    }).flatten().collect();

    let contents = results_wbs[0].get_value((0, 0)).expect("Cell A1 must at least begin with the date of the election.").to_string();
    let (date, name) = match extract_date_and_remainder(contents.as_str()) {
        Ok((date, title)) => (date, title.split("Official").collect::<Vec<_>>()[0].trim()),
        Err(why) => {
            emit(Log::Error(format!("Failed to get date from cell AI: {}", why.to_string())));
            return;
        }
    };

    let name = format!("{} {}", date.year(), name);

    println!("{} Adding {} to the election index (date detected as {}).", "Ready!".green().bold(), name.underline(), date.to_string().underline());
    emit(Log::Info("If this was not the desired name, delete it from the database and run again with the --name argument set."));
    let map_path: PathBuf = PathBuf::from(election_path).join("map");
    conn.execute("INSERT INTO election_info(name, date, map) VALUES(?1, ?2, ?3);", (name, date, map_path.display().to_string())).unwrap();
    let election_id = conn.last_insert_rowid();

    let mut county_abbr_lookup: HashMap<String, String> = HashMap::new(); // abbr -> name
    let mut county_lookup: HashMap<String, Rc<County>> = HashMap::new();
    for row in 0..county_wb.get_size().0 {
        let row = row as u32;

        let (abbr, name) = match (county_wb.get_value((row, 0)), county_wb.get_value((row, 1))) {
            (Some(abbr), Some(name))=> (abbr.to_string(), name.to_string()),
            _ => return emit(Log::Error(format!("Missing county abbreviation or name in precinct-conversions#counties row={}", row)))
        };

        conn.execute("INSERT INTO county(name, electionId) VALUES(?1, ?2)", (name.clone(), election_id)).unwrap();
        let county = County {
            name: name.clone(),
            id: conn.last_insert_rowid()
        };

        county_abbr_lookup.insert(abbr, name.clone());
        county_lookup.insert(name, Rc::new(county));
    }

    let mut municipal_lookup: HashMap<String, Rc<Municipality>> = HashMap::new(); // fips -> municipality
    for row in 0..municipal_wb.get_size().0 {
        let row = row as u32;

        let (name, r#type, fips) = match (municipal_wb.get_value((row, 0)), municipal_wb.get_value((row, 1)), municipal_wb.get_value((row, 3))) {
            (Some(name), Some(r#type),Some(fips)) => (name.to_string(), r#type.to_string(), fips.to_string()),
            _ => return emit(Log::Error(format!("Missing name, type, or FIPS code in municipal-codes row={}", row)))
        };

        municipal_lookup.insert(fips.clone(), Rc::new(Municipality {
            name,
            r#type: match r#type.as_str() {
                "city/village" => MunicipalType::City,
                "township" => MunicipalType::Township,
                _ => return emit(Log::Error(format!("Unknown municipal type {} in municipal-codes row={}", r#type.underline(), row)))
            },
            fips,
            precincts: Rc::new(RefCell::new(Vec::new())),
            merges: Vec::new()
        }));
    }

    for row in 0..precinct_wb.get_size().0 {
        let row = row as u32;

        let (county, name) = match (precinct_wb.get_value((row, 0)), precinct_wb.get_value((row, 1))) {
            (Some(county), Some(name)) => {
                let county = match county_lookup.get(&county.to_string()) {
                    Some(county) => Rc::clone(county),
                    None => return emit(Log::Error(format!("Unable to find county {} on precinct-conversions#precincts row={}", county.to_string().underline(), row)))
                };

                (county, name.to_string())
            },

            _ => return emit(Log::Error(format!("Missing county or name in precinct-conversions#precincts row={}", row)))
        };

        let precinct = Precinct {
            name: name.clone(), county: Rc::clone(&county), row_id: row
        };

        let mut fips_codes = Vec::new();
        for col in 2..precinct_wb.get_size().1 {
            let col = col as u32;

            let fips = precinct_wb.get_value((row, col)).unwrap().to_string();
            if fips.is_empty() {
                break
            } else {
                fips_codes.push(fips);
            }
        }

        let mut municis = Vec::new();
        for fips in &fips_codes {
            municis.push(match municipal_lookup.get(fips) {
                Some(boxed) => Rc::clone(boxed),
                None => return emit(Log::Error(format!("Unable to find municipality from FIPS code {} on precinct-conversions#precincts row={}", fips.underline(), row)))
            });
        }

        if fips_codes.len() > 1 {
            let mut municis_names: Vec<String> = Vec::new();
            municis_names.push(municis[0].name.clone());
            /*municis.iter().for_each(|muni| {
                if fips_codes.contains(&muni.fips) {
                    municis_names.push(muni.name.clone());
                }
            });*/
            let mut idx = 1;
            for _ in idx..fips_codes.len() {
                let muni = &municis[idx];
                if fips_codes.contains(&muni.fips) {
                    municis_names.push(muni.name.clone());
                }

                idx = idx + 1;
            }

            let mut precincts: HashSet<Rc<Precinct>> = HashSet::new();
            let mut merges: HashSet<String> = HashSet::new();

            municis.iter().for_each(|muni| {
                for p in &*muni.precincts.borrow() {
                    precincts.insert(Rc::clone(&p));
                }

                merges.insert(muni.fips.clone());
            });

            let new_munc = Rc::new(Municipality {
                name: municis_names.join(" + "),
                fips: fips_codes.join(","),
                r#type: MunicipalType::Mixed,
                precincts: Rc::new(RefCell::new(precincts.into_iter().collect())),
                merges: merges.into_iter().collect()
            });

            for fips in &fips_codes {
                municipal_lookup.remove(fips);
                municipal_lookup.insert(fips.clone(), Rc::clone(&new_munc));
            }
        }

        if fips_codes.len() == 0 {
            return emit(Log::Error(format!("Precinct {} in {} County not assigned to municipality on precinct-conversions#precincts row={}", name, county.name, row)));
        }

        let munc = match municipal_lookup.get(&fips_codes[0]) { // at this point this is the only municipality present or all the fips codes now point to the same merged entity
            Some(munc) => Rc::clone(munc),
            None => return emit(Log::Error(format!("Unable to find municipality with FIPS code {} on precinct-conversions#precincts row={}", fips_codes[0].underline(), row)))
        };

        munc.precincts.borrow_mut().push(Rc::new(precinct));
    }

    write!(File::create("muni.dump").unwrap(), "{:#?}", municipal_lookup).unwrap();

    todo!();

    conn.commit().unwrap();
}

fn extract_date_and_remainder(input: &str) -> Result<(chrono::NaiveDate, &str), chrono::ParseError> {
    use chrono::{NaiveDate};
    use chrono::format::{ParseError, ParseErrorKind};

    let format = "%B %d, %Y";
    
    if let Some(comma_pos) = input.find(',') {
        let date_str = &input[..comma_pos + 6];

        let date = NaiveDate::parse_from_str(date_str, format)?;

        let remainder = &input[comma_pos + 7..];

        Ok((date, remainder))
    } else {
        match NaiveDate::parse_from_str("November 5", format) {
            Ok(_) => panic!("This statement can never be reached."),
            Err(why) => Err(why)
        } // trigger ParseError::NotEnough
    }
}

fn find_matching_files(dir: &std::path::Path, pattern: &str) -> Vec<std::path::PathBuf> {
    use std::fs;

    let mut results = Vec::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_file() {
                if let Some(file_name) = path.file_name() {
                    if let Some(name_str) = file_name.to_str() {
                        if name_str.starts_with(pattern) && name_str.ends_with(".xlsx") {
                            results.push(path.clone());
                        }
                    }
                }
            }
        }
    }

    results
}