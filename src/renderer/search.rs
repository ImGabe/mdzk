use anyhow::{Context, Result};
use elasticlunr::Index;
use mdbook::{
    book::{Book, BookItem},
    config::Search,
};
use pulldown_cmark::*;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Creates all files required for search.
pub fn create_files(search_config: &Search, destination: &Path, book: &Book) -> Result<()> {
    let mut index = Index::new(&["title", "body", "breadcrumbs"]);
    let mut doc_urls = Vec::with_capacity(book.sections.len());

    debug!("Populating search index");
    for item in book.iter() {
        populate_index(&mut index, search_config, &mut doc_urls, item)?;
    }

    debug!("Writing search index as JSON");
    let index = write_to_json(index, search_config, doc_urls)?;
    if index.len() > 10_000_000 {
        warn!("searchindex.json is very large ({} bytes)", index.len());
    }
    crate::utils::write_file(&destination.join("searchindex.json"), index.as_bytes())?;
    crate::utils::write_file(
        &destination.join("searchindex.js"),
        format!("Object.assign(window.search, {});", index).as_bytes(),
    )?;

    debug!("Copying search JS files");
    let js_dir = destination.join("js");
    crate::utils::write_file(
        &js_dir.join("searcher.js"),
        include_bytes!("theme/js/searcher.js"),
    )?;
    crate::utils::write_file(
        &js_dir.join("mark.min.js"),
        include_bytes!("theme/js/mark.min.js"),
    )?;
    crate::utils::write_file(
        &js_dir.join("elasticlunr.min.js"),
        include_bytes!("theme/js/elasticlunr.min.js"),
    )?;

    Ok(())
}

fn write_to_json(index: Index, search_config: &Search, doc_urls: Vec<String>) -> Result<String> {
    use elasticlunr::config::{SearchBool, SearchOptions, SearchOptionsField};
    use std::collections::BTreeMap;

    #[derive(Serialize)]
    struct ResultsOptions {
        limit_results: u32,
        teaser_word_count: u32,
    }

    #[derive(Serialize)]
    struct SearchindexJson {
        /// The options used for displaying search results
        results_options: ResultsOptions,
        /// The searchoptions for elasticlunr.js
        search_options: SearchOptions,
        /// Used to lookup a document's URL from an integer document ref.
        doc_urls: Vec<String>,
        /// The index for elasticlunr.js
        index: elasticlunr::Index,
    }

    let mut fields = BTreeMap::new();
    let mut opt = SearchOptionsField {
        boost: Some(search_config.boost_title),
        ..SearchOptionsField::default()
    };
    fields.insert("title".into(), opt);
    opt.boost = Some(search_config.boost_paragraph);
    fields.insert("body".into(), opt);
    opt.boost = Some(search_config.boost_hierarchy);
    fields.insert("breadcrumbs".into(), opt);

    let search_options = SearchOptions {
        bool: if search_config.use_boolean_and {
            SearchBool::And
        } else {
            SearchBool::Or
        },
        expand: search_config.expand,
        fields,
    };

    let results_options = ResultsOptions {
        limit_results: search_config.limit_results,
        teaser_word_count: search_config.teaser_word_count,
    };

    let json_contents = SearchindexJson {
        results_options,
        search_options,
        doc_urls,
        index,
    };

    // By converting to serde_json::Value as an intermediary, we use a
    // BTreeMap internally and can force a stable ordering of map keys.
    let json_contents = serde_json::to_value(&json_contents)?;
    let json_contents = serde_json::to_string(&json_contents)?;

    Ok(json_contents)
}

/// Renders markdown into flat unformatted text and adds it to the search index.
fn populate_index(
    index: &mut Index,
    search_config: &Search,
    doc_urls: &mut Vec<String>,
    item: &BookItem,
) -> Result<()> {
    let chapter = match *item {
        BookItem::Chapter(ref ch) if !ch.is_draft_chapter() => ch,
        _ => return Ok(()),
    };

    let chapter_path = chapter
        .path
        .as_ref()
        .expect("Checked that path exists above");
    let filepath = Path::new(&chapter_path).with_extension("html");
    let filepath = filepath
        .to_str()
        .with_context(|| format!("Could not convert HTML path {:?} to str", filepath))?;
    let anchor_base = mdbook::utils::fs::normalize_path(filepath);

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    let mut p = Parser::new_ext(&chapter.content, opts).peekable();

    let mut breadcrumbs = chapter.parent_names.clone();
    breadcrumbs.push(chapter.name.clone());

    let mut in_heading = false;
    let max_section_depth = u32::from(search_config.heading_split_level);
    let mut section_id = None;
    let mut heading = String::new();
    let mut body = String::new();
    let mut footnote_numbers = HashMap::new();

    while let Some(event) = p.next() {
        match event {
            Event::Start(Tag::Heading(i, ..)) if i as u32 <= max_section_depth => {
                match (body.is_empty(), heading.is_empty()) {
                    (false, true) => {
                        // Content before first heading. Add it to the index under the note title.
                        add_to_index(
                            index,
                            doc_urls,
                            &anchor_base,
                            &None,
                            &[&chapter.name, &body, &breadcrumbs.join(" » ")],
                        );
                        body.clear();
                    }
                    (_, false) => {
                        // Section finished, the next heading is following now
                        // Write the data to the index, and clear it for the next section
                        add_to_index(
                            index,
                            doc_urls,
                            &anchor_base,
                            &section_id,
                            &[&heading, &body, &breadcrumbs.join(" » ")],
                        );
                        section_id = None;
                        heading.clear();
                        body.clear();
                        breadcrumbs.pop();
                    }
                    _ => {}
                }
                in_heading = true;
            }
            Event::End(Tag::Heading(i, ..)) if i as u32 <= max_section_depth => {
                in_heading = false;
                section_id = Some(mdbook::utils::id_from_content(&heading));
                breadcrumbs.push(heading.clone());
            }
            Event::Start(Tag::FootnoteDefinition(name)) => {
                let number = footnote_numbers.len() + 1;
                footnote_numbers.entry(name).or_insert(number);
            }
            Event::Html(html) => {
                let mut html_block = html.into_string();

                // As of pulldown_cmark 0.6, html events are no longer contained
                // in an HtmlBlock tag. We must collect consecutive Html events
                // into a block ourselves.
                while let Some(Event::Html(html)) = p.peek() {
                    html_block.push_str(html);
                    p.next();
                }

                body.push_str(&clean_html(&html_block));
            }
            Event::Start(Tag::Link(_, _, title)) => {
                // Push only link titles, not the whole markup
                body.push_str(&title);
            }
            Event::Start(_) | Event::End(_) | Event::Rule | Event::SoftBreak | Event::HardBreak => {
                // Insert spaces where HTML output would usually separate text
                // to ensure words don't get merged together
                if in_heading {
                    heading.push(' ');
                } else {
                    body.push(' ');
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if in_heading {
                    heading.push_str(&text);
                } else {
                    body.push_str(&text);
                }
            }
            Event::FootnoteReference(name) => {
                let len = footnote_numbers.len() + 1;
                let number = footnote_numbers.entry(name).or_insert(len);
                body.push_str(&format!(" [{}] ", number));
            }
            Event::TaskListMarker(_checked) => {}
        }
    }

    if !body.is_empty() || !heading.is_empty() {
        if heading.is_empty() {
            if let Some(chapter) = breadcrumbs.first() {
                heading = chapter.clone();
            }
        }
        // Make sure the last section is added to the index
        add_to_index(
            index,
            doc_urls,
            &anchor_base,
            &section_id,
            &[&heading, &body, &breadcrumbs.join(" » ")],
        );
    }

    Ok(())
}

/// Uses the given arguments to construct a search document, then inserts it to the given index.
fn add_to_index(
    index: &mut Index,
    doc_urls: &mut Vec<String>,
    anchor_base: &str,
    section_id: &Option<String>,
    items: &[&str],
) {
    let url = if let Some(ref id) = *section_id {
        Cow::Owned(format!("{}#{}", anchor_base, id))
    } else {
        Cow::Borrowed(anchor_base)
    };
    let url = mdbook::utils::collapse_whitespace(url.trim());
    let doc_ref = doc_urls.len().to_string();
    doc_urls.push(url.into());

    let items = items
        .iter()
        .map(|&x| mdbook::utils::collapse_whitespace(x.trim()));
    index.add_doc(&doc_ref, items);
}

fn clean_html(html: &str) -> String {
    lazy_static::lazy_static! {
        static ref AMMONIA: ammonia::Builder<'static> = {
            let mut clean_content = HashSet::new();
            clean_content.insert("script");
            clean_content.insert("style");
            let mut builder = ammonia::Builder::new();
            builder
                .tags(HashSet::new())
                .tag_attributes(HashMap::new())
                .generic_attributes(HashSet::new())
                .link_rel(None)
                .allowed_classes(HashMap::new())
                .clean_content_tags(clean_content);
            builder
        };
    }
    AMMONIA.clean(html).to_string()
}
