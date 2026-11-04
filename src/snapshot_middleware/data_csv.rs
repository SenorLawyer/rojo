//! Turns `*.data.csv` spreadsheets into ModuleScripts that return their rows
//! as Luau tables, so game configuration can be edited in a spreadsheet
//! instead of by hand in Lua.

use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write as _,
    path::Path,
};

use anyhow::{bail, Context};
use memofs::Vfs;
use rbx_dom_weak::ustr;

use crate::snapshot::{InstanceContext, InstanceMetadata, InstanceSnapshot};

use super::{
    dir::snapshot_dir_no_meta,
    meta_file::{AdjacentMetadata, DirectoryMetadata},
};

/// The column that turns the returned table into a dictionary keyed by its
/// values. Without it, the module returns an array of rows.
const ID_COLUMN: &str = "id";

pub fn snapshot_data_csv(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let contents = vfs.read(path)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    let source = data_csv_to_luau(&contents, file_name)
        .with_context(|| format!("Could not convert data CSV file {}", path.display()))?;

    let mut snapshot = InstanceSnapshot::new()
        .name(name)
        .class_name("ModuleScript")
        .property(ustr("Source"), source)
        .metadata(
            InstanceMetadata::new()
                .instigating_source(path)
                .relevant_paths(vec![vfs.canonicalize(path)?])
                .context(context),
        );

    AdjacentMetadata::read_and_apply_all(vfs, path, name, &mut snapshot)?;

    Ok(Some(snapshot))
}

/// Snapshots a directory containing an `init.data.csv` file as a
/// ModuleScript, the same way `init.luau` does.
pub fn snapshot_data_csv_init(
    context: &InstanceContext,
    vfs: &Vfs,
    init_path: &Path,
    name: &str,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let folder_path = init_path.parent().unwrap();
    let dir_snapshot = snapshot_dir_no_meta(context, vfs, folder_path, name)?.unwrap();

    if dir_snapshot.class_name != "Folder" {
        anyhow::bail!(
            "init.data.csv can only be used if the instance produced by \
             the containing directory would be a Folder.\n\
             \n\
             The directory {} turned into an instance of class {}.",
            folder_path.display(),
            dir_snapshot.class_name
        );
    }

    let mut init_snapshot =
        snapshot_data_csv(context, vfs, init_path, &dir_snapshot.name)?.unwrap();

    init_snapshot.children = dir_snapshot.children;
    init_snapshot.metadata = dir_snapshot.metadata;

    DirectoryMetadata::read_and_apply_all(vfs, folder_path, &mut init_snapshot)?;

    Ok(Some(init_snapshot))
}

/// A single parsed cell. Empty cells are `None` and are left out of the row.
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Number(f64),
    Bool(bool),
    String(String),
}

impl Cell {
    fn parse(value: &str) -> Option<Self> {
        if value.is_empty() {
            return None;
        }

        if value.eq_ignore_ascii_case("true") {
            return Some(Self::Bool(true));
        }
        if value.eq_ignore_ascii_case("false") {
            return Some(Self::Bool(false));
        }

        // Rust's float parser also accepts `inf` and `NaN`, which have no
        // literal in Luau and are much more likely to be meant as text.
        if let Ok(number) = value.parse::<f64>() {
            if number.is_finite() {
                return Some(Self::Number(number));
            }
        }

        Some(Self::String(value.to_owned()))
    }

    fn type_name(&self) -> &'static str {
        match self {
            Self::Number(_) => "number",
            Self::Bool(_) => "boolean",
            Self::String(_) => "string",
        }
    }

    fn write_luau(&self, output: &mut String) {
        match self {
            Self::Number(number) => write_number(output, *number),
            Self::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Self::String(value) => write_string(output, value),
        }
    }

    fn write_key(&self, output: &mut String) {
        match self {
            Self::String(value) if is_valid_ident(value) => output.push_str(value),
            _ => {
                output.push('[');
                self.write_luau(output);
                output.push(']');
            }
        }
    }
}

struct Column {
    /// Index of the column in each CSV record.
    index: usize,
    name: String,
    types: BTreeSet<&'static str>,
    optional: bool,
}

/// Converts the contents of a `.data.csv` file into the source of a
/// ModuleScript.
///
/// The first record holds the column names. If there is a column named `id`,
/// the module returns a dictionary keyed by it, in file order. Otherwise it
/// returns an array of rows.
fn data_csv_to_luau(contents: &[u8], file_name: &str) -> anyhow::Result<String> {
    // Ragged rows are checked by hand so the error can say which row is wrong
    // in terms a spreadsheet user understands.
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(contents);

    let headers = reader.headers().context("could not read the header row")?;
    let header_count = headers.len();

    let mut columns: Vec<Column> = Vec::new();
    let mut seen_headers = HashMap::new();
    for (index, header) in headers.iter().enumerate() {
        // Spreadsheet exports often add trailing columns without a header.
        // These hold no named data, so they are skipped like LocalizationTable
        // CSVs skip them.
        if header.is_empty() {
            continue;
        }

        if let Some(previous) = seen_headers.insert(header.to_owned(), index) {
            bail!(
                "the header row names column {header:?} twice (columns {} and {})",
                previous + 1,
                index + 1
            );
        }

        columns.push(Column {
            index,
            name: header.to_owned(),
            types: BTreeSet::new(),
            optional: false,
        });
    }

    let id_column = columns.iter().position(|column| column.name == ID_COLUMN);

    let mut rows: Vec<Vec<Option<Cell>>> = Vec::new();
    let mut ids: HashMap<String, u64> = HashMap::new();

    for record in reader.records() {
        let record = record.context("could not read a row")?;
        let line = record.position().map(|pos| pos.line()).unwrap_or_default();

        if record.len() != header_count {
            bail!(
                "row on line {line} has {} fields, but the header row has {header_count}",
                record.len()
            );
        }

        let mut row = Vec::with_capacity(columns.len());
        for column in &mut columns {
            let cell = Cell::parse(&record[column.index]);
            match &cell {
                Some(cell) => {
                    column.types.insert(cell.type_name());
                }
                None => column.optional = true,
            }
            row.push(cell);
        }

        if let Some(id_column) = id_column {
            let Some(id) = &row[id_column] else {
                bail!("row on line {line} has an empty {ID_COLUMN:?} value");
            };

            // Keys are compared as they will appear in Luau, so `1` and `1.0`
            // count as the same id.
            let mut key = String::new();
            id.write_key(&mut key);
            if let Some(previous_line) = ids.insert(key, line) {
                bail!(
                    "row on line {line} has the same {ID_COLUMN:?} as the row on \
                     line {previous_line}: {:?}",
                    &record[columns[id_column].index]
                );
            }
        }

        rows.push(row);
    }

    let mut output = String::new();
    output.push_str("--!strict\n");
    if file_name.is_empty() {
        output.push_str("-- Generated by Rojo from a data CSV file. Edit the CSV file instead.\n");
    } else {
        writeln!(
            output,
            "-- Generated by Rojo from {file_name}. Edit the CSV file instead."
        )
        .unwrap();
    }
    output.push('\n');

    output.push_str("export type Row = {\n");
    for column in &columns {
        output.push('\t');
        write_field_name(&mut output, &column.name);
        output.push_str(": ");
        output.push_str(&column_type(column));
        output.push_str(",\n");
    }
    output.push_str("}\n\n");

    match id_column {
        Some(id_column) => {
            let key_type = match columns[id_column].types.len() {
                0 => "string".to_owned(),
                _ => join_types(&columns[id_column].types),
            };
            writeln!(output, "local data: {{ [{key_type}]: Row }} = {{").unwrap();
        }
        None => output.push_str("local data: { Row } = {\n"),
    }

    for row in &rows {
        output.push('\t');
        if let Some(id_column) = id_column {
            // Checked while reading the rows.
            row[id_column].as_ref().unwrap().write_key(&mut output);
            output.push_str(" = ");
        }
        write_row(&mut output, &columns, row);
        output.push_str(",\n");
    }

    output.push_str("}\n\nreturn data\n");

    Ok(output)
}

fn write_row(output: &mut String, columns: &[Column], row: &[Option<Cell>]) {
    let mut fields = columns
        .iter()
        .zip(row)
        .filter_map(|(column, cell)| Some((column, cell.as_ref()?)))
        .peekable();

    if fields.peek().is_none() {
        output.push_str("{}");
        return;
    }

    output.push_str("{ ");
    let mut first = true;
    for (column, cell) in fields {
        if !first {
            output.push_str(", ");
        }
        first = false;

        write_field_name(output, &column.name);
        output.push_str(" = ");
        cell.write_luau(output);
    }
    output.push_str(" }");
}

fn column_type(column: &Column) -> String {
    if column.types.is_empty() {
        // Every cell in the column is empty, so it is always nil.
        return "nil".to_owned();
    }

    let types = join_types(&column.types);
    match (column.optional, column.types.len()) {
        (false, _) => types,
        (true, 1) => format!("{types}?"),
        (true, _) => format!("({types})?"),
    }
}

fn join_types(types: &BTreeSet<&'static str>) -> String {
    types.iter().copied().collect::<Vec<_>>().join(" | ")
}

/// Writes a table field name or table type property, quoting it when it is
/// not a valid identifier.
fn write_field_name(output: &mut String, name: &str) {
    if is_valid_ident(name) {
        output.push_str(name);
    } else {
        output.push('[');
        write_string(output, name);
        output.push(']');
    }
}

fn write_number(output: &mut String, number: f64) {
    // Rust prints finite floats without an exponent and without a trailing
    // `.0` for whole numbers, which is valid Luau.
    write!(output, "{number}").unwrap();
}

fn write_string(output: &mut String, value: &str) {
    output.push('"');
    for c in value.chars() {
        match c {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            // Three digits, so a digit that follows is not read as part of
            // the escape.
            c if c.is_ascii_control() => write!(output, "\\{:03}", c as u32).unwrap(),
            c => output.push(c),
        }
    }
    output.push('"');
}

fn is_valid_ident(value: &str) -> bool {
    // Luau's contextual keywords (`continue`, `type`, `export`) are valid
    // field names, so only the reserved words are listed.
    const KEYWORDS: &[&str] = &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in",
        "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ];

    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }

    chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && !KEYWORDS.contains(&value)
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};

    fn convert(contents: &str) -> String {
        data_csv_to_luau(contents.as_bytes(), "items.data.csv").unwrap()
    }

    fn convert_err(contents: &str) -> String {
        format!(
            "{:#}",
            data_csv_to_luau(contents.as_bytes(), "items.data.csv").unwrap_err()
        )
    }

    #[test]
    fn rows_with_id_become_a_dictionary() {
        let source = convert(
            "id,name,cost,enabled\n\
             sword,Sword,100,TRUE\n\
             iron-shield,Iron Shield,12.5,false\n\
             42,Answer,,\n",
        );

        assert_eq!(
            source,
            "--!strict\n\
             -- Generated by Rojo from items.data.csv. Edit the CSV file instead.\n\
             \n\
             export type Row = {\n\
             \tid: number | string,\n\
             \tname: string,\n\
             \tcost: number?,\n\
             \tenabled: boolean?,\n\
             }\n\
             \n\
             local data: { [number | string]: Row } = {\n\
             \tsword = { id = \"sword\", name = \"Sword\", cost = 100, enabled = true },\n\
             \t[\"iron-shield\"] = { id = \"iron-shield\", name = \"Iron Shield\", cost = 12.5, enabled = false },\n\
             \t[42] = { id = 42, name = \"Answer\" },\n\
             }\n\
             \n\
             return data\n"
        );
    }

    #[test]
    fn rows_without_id_become_an_array() {
        let source = convert("name,weight\nApple,0.2\nRock,3\n");

        assert_eq!(
            source,
            "--!strict\n\
             -- Generated by Rojo from items.data.csv. Edit the CSV file instead.\n\
             \n\
             export type Row = {\n\
             \tname: string,\n\
             \tweight: number,\n\
             }\n\
             \n\
             local data: { Row } = {\n\
             \t{ name = \"Apple\", weight = 0.2 },\n\
             \t{ name = \"Rock\", weight = 3 },\n\
             }\n\
             \n\
             return data\n"
        );
    }

    #[test]
    fn values_are_parsed_by_type() {
        assert_eq!(Cell::parse(""), None);
        assert_eq!(Cell::parse("1e3"), Some(Cell::Number(1000.0)));
        assert_eq!(Cell::parse("-4"), Some(Cell::Number(-4.0)));
        assert_eq!(Cell::parse("True"), Some(Cell::Bool(true)));
        assert_eq!(Cell::parse("FALSE"), Some(Cell::Bool(false)));
        assert_eq!(Cell::parse("inf"), Some(Cell::String("inf".into())));
        assert_eq!(Cell::parse("NaN"), Some(Cell::String("NaN".into())));
        assert_eq!(Cell::parse(" 5"), Some(Cell::String(" 5".into())));
        assert_eq!(Cell::parse("yes"), Some(Cell::String("yes".into())));
    }

    #[test]
    fn strings_and_keys_are_escaped() {
        let source = convert(
            "id,display name,end,type\n\
             \"quote\"\"d\",\"line\nbreak\",back\\slash,\"tab\tbell\x07\"\n",
        );

        assert!(
            source.contains("\t[\"display name\"]: string,\n"),
            "{source}"
        );
        assert!(source.contains("\t[\"end\"]: string,\n"), "{source}");
        assert!(source.contains("\ttype: string,\n"), "{source}");
        assert!(
            source.contains(
                "\t[\"quote\\\"d\"] = { id = \"quote\\\"d\", [\"display name\"] = \"line\\nbreak\", \
                 [\"end\"] = \"back\\\\slash\", type = \"tab\\tbell\\007\" },\n"
            ),
            "{source}"
        );
    }

    #[test]
    fn mixed_and_empty_columns_have_union_types() {
        let source = convert("value,blank\n1,\nhello,\n,\n");

        assert!(
            source.contains("\tvalue: (number | string)?,\n"),
            "{source}"
        );
        assert!(source.contains("\tblank: nil,\n"), "{source}");
        assert!(source.contains("\t{},\n"), "{source}");
    }

    #[test]
    fn headers_only_returns_an_empty_table() {
        let source = convert("id,name\n");
        assert!(
            source.contains("local data: { [string]: Row } = {\n}\n"),
            "{source}"
        );
    }

    #[test]
    fn columns_without_a_header_are_skipped() {
        let source = convert("name,\nApple,ignored\n");
        assert!(source.contains("\t{ name = \"Apple\" },\n"), "{source}");
        assert!(!source.contains("ignored"), "{source}");
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let error = convert_err("id,name\n1,One\n2,Two\n1.0,Also one\n");
        assert_eq!(
            error,
            "row on line 4 has the same \"id\" as the row on line 2: \"1.0\""
        );
    }

    #[test]
    fn empty_ids_are_rejected() {
        let error = convert_err("id,name\nsword,Sword\n,Nameless\n");
        assert_eq!(error, "row on line 3 has an empty \"id\" value");
    }

    #[test]
    fn ragged_rows_are_rejected() {
        let error = convert_err("id,name\nsword,Sword\nshield\n");
        assert_eq!(
            error,
            "row on line 3 has 1 fields, but the header row has 2"
        );
    }

    #[test]
    fn duplicate_headers_are_rejected() {
        let error = convert_err("name,cost,name\n");
        assert_eq!(
            error,
            "the header row names column \"name\" twice (columns 1 and 3)"
        );
    }

    #[test]
    fn errors_name_the_file() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/items.data.csv", VfsSnapshot::file("id\na\na\n"))
            .unwrap();

        let error = snapshot_data_csv(
            &InstanceContext::default(),
            &Vfs::new(imfs),
            Path::new("/items.data.csv"),
            "items",
        )
        .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "Could not convert data CSV file /items.data.csv: row on line 3 has the \
             same \"id\" as the row on line 2: \"a\""
        );
    }

    #[test]
    fn data_csv_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/items.data.csv",
            VfsSnapshot::file("id,cost\nsword,100\nshield,50\n"),
        )
        .unwrap();

        let instance_snapshot = snapshot_data_csv(
            &InstanceContext::default(),
            &Vfs::new(imfs),
            Path::new("/items.data.csv"),
            "items",
        )
        .unwrap()
        .unwrap();

        insta::assert_yaml_snapshot!(instance_snapshot);
    }
}
