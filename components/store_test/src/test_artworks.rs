use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use nlp::Language;
use store::{
    document::{DocumentBuilder, IndexOptions, OptionValue},
    Comparator, ComparisonOperator, DocumentId, FieldValue, Filter, Store, TextQuery,
};

const FIELDS: [&str; 20] = [
    "id",
    "accession_number",
    "artist",
    "artistRole",
    "artistId",
    "title",
    "dateText",
    "medium",
    "creditLine",
    "year",
    "acquisitionYear",
    "dimensions",
    "width",
    "height",
    "depth",
    "units",
    "inscription",
    "thumbnailCopyright",
    "thumbnailUrl",
    "url",
];

enum FieldType {
    Keyword,
    Text,
    FullText,
    Integer,
}

const FIELDS_OPTIONS: [FieldType; 20] = [
    FieldType::Integer,  // "id",
    FieldType::Keyword,  // "accession_number",
    FieldType::Text,     // "artist",
    FieldType::Keyword,  // "artistRole",
    FieldType::Integer,  // "artistId",
    FieldType::FullText, // "title",
    FieldType::FullText, // "dateText",
    FieldType::FullText, // "medium",
    FieldType::FullText, // "creditLine",
    FieldType::Integer,  // "year",
    FieldType::Integer,  // "acquisitionYear",
    FieldType::FullText, // "dimensions",
    FieldType::Integer,  // "width",
    FieldType::Integer,  // "height",
    FieldType::Integer,  // "depth",
    FieldType::Text,     // "units",
    FieldType::FullText, // "inscription",
    FieldType::Text,     // "thumbnailCopyright",
    FieldType::Text,     // "thumbnailUrl",
    FieldType::Text,     // "url",
];

pub fn insert_artworks<'x, T, I>(db: &T)
where
    T: Store<'x, I>,
    I: Iterator<Item = DocumentId>,
{
    rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build()
        .unwrap()
        .scope(|s| {
            let db = Arc::new(db);
            let documents = Arc::new(Mutex::new(Vec::new()));

            for record in csv::ReaderBuilder::new()
                .has_headers(true)
                .from_path("/terastore/datasets/artwork_data.csv")
                .unwrap()
                .records()
                .into_iter()
            {
                let record = record.unwrap();
                let documents = documents.clone();
                s.spawn(move |_| {
                    let mut builder = DocumentBuilder::new();
                    for (pos, field) in record.iter().enumerate() {
                        if field.is_empty() {
                            continue;
                        }

                        match FIELDS_OPTIONS[pos] {
                            FieldType::Text => {
                                builder.add_text(
                                    pos as u8,
                                    field.to_lowercase().into(),
                                    <OptionValue>::Sortable,
                                );
                            }
                            FieldType::FullText => {
                                builder.add_full_text(
                                    pos as u8,
                                    field.to_lowercase().into(),
                                    Some(Language::English),
                                    <OptionValue>::Sortable,
                                );
                            }
                            FieldType::Integer => {
                                if let Ok(value) = field.parse::<u32>() {
                                    builder.add_integer(
                                        pos as u8,
                                        value,
                                        <OptionValue>::Sortable | <OptionValue>::Stored,
                                    );
                                }
                            }
                            FieldType::Keyword => {
                                builder.add_keyword(
                                    pos as u8,
                                    field.to_lowercase().into(),
                                    <OptionValue>::Sortable | <OptionValue>::Stored,
                                );
                            }
                        }
                    }
                    documents.lock().unwrap().push(builder);
                });
            }

            let mut documents = documents.lock().unwrap();
            let documents_len = documents.len();
            let mut document_chunk = Vec::new();

            println!("Parsed {} entries.", documents_len);

            for (pos, document) in documents.drain(..).enumerate() {
                document_chunk.push(document);
                if document_chunk.len() == 1000 || pos == documents_len - 1 {
                    let db = db.clone();
                    let chunk = document_chunk;
                    document_chunk = Vec::new();

                    s.spawn(move |_| {
                        let now = Instant::now();
                        let doc_ids = db.insert_bulk(0, 0, chunk).unwrap();
                        println!(
                            "Inserted {} entries in {} ms (Thread {}/{}).",
                            doc_ids.len(),
                            now.elapsed().as_millis(),
                            rayon::current_thread_index().unwrap(),
                            rayon::current_num_threads()
                        );
                    });
                }
            }
        });
}

pub fn filter_artworks<'x, T: 'x, I>(db: &'x T)
where
    T: Store<'x, I>,
    I: Iterator<Item = DocumentId>,
{
    let mut fields = HashMap::new();
    for (field_num, field) in FIELDS.iter().enumerate() {
        fields.insert(field.to_string(), field_num as u8);
    }

    let tests = [
        /*(
            Filter::and(vec![
                Filter::new_condition(
                    fields["title"],
                    ComparisonOperator::Equal,
                    FieldValue::FullText("water"),
                ),
                Filter::new_condition(
                    fields["year"],
                    ComparisonOperator::Equal,
                    FieldValue::Integer(1979),
                ),
            ]),
            vec!["p11293"],
        ),
        (
            Filter::and(vec![
                Filter::new_condition(
                    fields["medium"],
                    ComparisonOperator::Equal,
                    FieldValue::FullText("gelatin"),
                ),
                Filter::new_condition(
                    fields["year"],
                    ComparisonOperator::GreaterThan,
                    FieldValue::Integer(2000),
                ),
                Filter::new_condition(
                    fields["width"],
                    ComparisonOperator::LowerThan,
                    FieldValue::Integer(180),
                ),
            ]),
            vec!["p79426", "p79427", "p79428", "p79429", "p79430"],
        ),
        (
            Filter::and(vec![
                Filter::new_condition(
                    fields["title"],
                    ComparisonOperator::Equal,
                    FieldValue::FullText("'rustic bridge'"),
                ),
            ]),
            vec!["d05503"],
        ),
        (
            Filter::and(vec![
                Filter::new_condition(
                    fields["title"],
                    ComparisonOperator::Equal,
                    FieldValue::FullText(TextQuery::query_english("'rustic'")),
                ),
                Filter::new_condition(
                    fields["title"],
                    ComparisonOperator::Equal,
                    FieldValue::FullText(TextQuery::query_english("study")),
                ),
            ]),
            vec!["d00399", "d05352"],
        ),
        (
            Filter::and(vec![
                Filter::new_condition(
                    fields["artist"],
                    ComparisonOperator::Equal,
                    FieldValue::Text("mauro kunst"),
                ),
                Filter::new_condition(
                    fields["artistRole"],
                    ComparisonOperator::Equal,
                    FieldValue::Keyword("artist"),
                ),
                Filter::or(vec![
                    Filter::new_condition(
                        fields["year"],
                        ComparisonOperator::Equal,
                        FieldValue::Integer(1969),
                    ),
                    Filter::new_condition(
                        fields["year"],
                        ComparisonOperator::Equal,
                        FieldValue::Integer(1971),
                    ),
                ])
            ]),
            vec!["p01764", "t05843"],
        ),
        (
            Filter::and(vec![
                Filter::not(vec![Filter::new_condition(
                    fields["medium"],
                    ComparisonOperator::Equal,
                    FieldValue::FullText(TextQuery::query_english("oil")),
                )]),
                Filter::new_condition(
                    fields["creditLine"],
                    ComparisonOperator::Equal,
                    FieldValue::FullText(TextQuery::query_english("bequeath")),
                ),
                Filter::or(vec![
                    Filter::and(vec![
                        Filter::new_condition(
                            fields["year"],
                            ComparisonOperator::GreaterEqualThan,
                            FieldValue::Integer(1900),
                        ),
                        Filter::new_condition(
                            fields["year"],
                            ComparisonOperator::LowerThan,
                            FieldValue::Integer(1910),
                        ),
                    ]),
                    Filter::and(vec![
                        Filter::new_condition(
                            fields["year"],
                            ComparisonOperator::GreaterEqualThan,
                            FieldValue::Integer(2000),
                        ),
                        Filter::new_condition(
                            fields["year"],
                            ComparisonOperator::LowerThan,
                            FieldValue::Integer(2010),
                        ),
                    ]),
                ]),
            ]),
            vec![
                "n02478", "n02479", "n03568", "n03658", "n04327", "n04328", "n04721", "n04739",
                "n05095", "n05096", "n05145", "n05157", "n05158", "n05159", "n05298", "n05303",
                "n06070", "t01181", "t03571", "t05805", "t05806", "t12147", "t12154", "t12155",
            ],
        ),*/
        (
            Filter::and(vec![
                Filter::new_condition(
                    fields["artist"],
                    ComparisonOperator::Equal,
                    FieldValue::Text("warhol"),
                ),
                Filter::not(vec![Filter::new_condition(
                    fields["title"],
                    ComparisonOperator::Equal,
                    FieldValue::FullText(TextQuery::query_english("'campbell'")),
                )]),
                Filter::not(vec![Filter::or(vec![
                    Filter::new_condition(
                        fields["year"],
                        ComparisonOperator::GreaterThan,
                        FieldValue::Integer(1980),
                    ),
                    Filter::and(vec![
                        Filter::new_condition(
                            fields["width"],
                            ComparisonOperator::GreaterThan,
                            FieldValue::Integer(500),
                        ),
                        Filter::new_condition(
                            fields["height"],
                            ComparisonOperator::GreaterThan,
                            FieldValue::Integer(500),
                        ),
                    ]),
                ])]),
                Filter::new_condition(
                    fields["acquisitionYear"],
                    ComparisonOperator::Equal,
                    FieldValue::Integer(2008),
                ),
            ]),
            vec![""],
        ),
    ];

    for (filter, expected_results) in tests {
        let mut results = Vec::with_capacity(expected_results.len());

        for doc_id in db
            .query(
                0,
                0,
                Some(filter),
                Some(vec![Comparator::ascending(fields["accession_number"])]),
            )
            .unwrap()
        {
            results.push(
                db.get_text(0, 0, doc_id, fields["accession_number"])
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(results, expected_results);
    }
}

pub fn sort_artworks<'x, T: 'x, I>(db: &'x T)
where
    T: Store<'x, I>,
    I: Iterator<Item = DocumentId>,
{
    let mut fields = HashMap::new();
    for (field_num, field) in FIELDS.iter().enumerate() {
        fields.insert(field.to_string(), field_num as u8);
    }

    let tests = [
        (
            vec![
                Comparator::descending(fields["year"]),
                Comparator::ascending(fields["acquisitionYear"]),
                Comparator::ascending(fields["width"]),
                Comparator::descending(fields["accession_number"]),
            ],
            vec![
                "t13655", "t13731", "t13811", "p13323", "p13352", "p13351", "p13350", "p13349",
                "p13348", "p13347", "p13346", "p13345", "p13344", "p13342", "p13341", "p13340",
                "p13339", "p13338", "p13337", "p13336",
            ],
        ),
        (
            vec![
                Comparator::descending(fields["width"]),
                Comparator::ascending(fields["height"]),
            ],
            vec![
                "t03681", "t12601", "ar00166", "t12625", "t12915", "p04182", "t06483", "ar00703",
                "t07671", "ar00021", "t05557", "t07918", "p06298", "p05465", "p06640", "t12855",
                "t01355", "t12800", "t12557", "t02078",
            ],
        ),
        (
            vec![
                Comparator::descending(fields["medium"]),
                Comparator::descending(fields["artistRole"]),
            ],
            vec![
                "ar00627", "ar00052", "t00352", "t07275", "t12318", "t04931", "t13691", "t13690",
                "t13689", "t13688", "t13687", "t13686", "t13683", "t07766", "t07918", "t12993",
                "ar00044", "t13326", "t07614", "t12414",
            ],
        ),
    ];

    for (sort, expected_results) in tests {
        let mut results = Vec::with_capacity(expected_results.len());

        for doc_id in db.query(0, 0, None, Some(sort)).unwrap() {
            results.push(
                db.get_text(0, 0, doc_id, fields["accession_number"])
                    .unwrap()
                    .unwrap(),
            );

            if results.len() == expected_results.len() {
                break;
            }
        }
        assert_eq!(results, expected_results);
    }
}
