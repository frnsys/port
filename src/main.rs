use std::cmp::Reverse;
use std::collections::HashMap;
use std::os::unix::fs::symlink as symlink_dir;
use std::path::{Path, PathBuf};

use anyhow::Result;
use fs_err as fs;
use gray_matter::{engine::YAML, Matter};
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};
use time::format_description::well_known::Rfc2822;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};
use walkdir::{DirEntry, WalkDir};

// For parsing `published_at` datetimes.
time::serde::format_description!(
    serde_datetime,
    PrimitiveDateTime,
    "[month].[day].[year] [hour]:[minute]"
);

/// Directory name that will host static assets.
const ASSETS_DIR: &str = "assets";

struct Port {
    config: Config,
    templates: Tera,
}
impl Port {
    fn compile_post(&self, category: &str, path: &Path) -> Result<Post> {
        let slug = path
            .file_stem()
            .expect("Path should have a file stem")
            .to_string_lossy();
        let contents = fs::read_to_string(path)?;
        let (meta, body) = extract_metadata(&contents)?;

        let Compiled {
            html,
            title,
            body,
            main_image: image,
        } = compile_markdown(&body)?;

        Ok(Post {
            url: format!("{category}/{slug}"),
            slug: slug.to_string(),
            category: category.to_string(),
            html,
            meta,
            title,
            image,
            description: Some(if body.len() <= 140 {
                body.to_string()
            } else {
                format!("{}…", &body.chars().take(139).collect::<String>())
            }),
        })
    }

    fn build_index<'a>(&'a self, path: PathBuf, posts: &'a [Post]) -> Result<()> {
        let base = PathBuf::from("/").join(&path);
        let posts: Vec<_> = posts.iter().filter(|post| !post.meta.draft).collect();
        let pages = paginate(&posts, self.config.per_page).map(move |page| {
            let page_path = if page.page == 0 {
                path.to_path_buf()
            } else {
                path.join(format!("p/{}", page.page + 1))
            };
            let mut context = Context::new();
            context.insert("site", &self.config);
            context.insert("posts", page.posts);
            context.insert(
                "prev_page",
                &page.prev.map(|p| {
                    if p == 1 {
                        base.clone()
                    } else {
                        base.join(format!("p/{p}"))
                    }
                }),
            );
            context.insert("next_page", &page.next.map(|p| base.join(format!("p/{p}"))));
            context.insert("current_url", &page_path);
            (
                page_path,
                self.templates
                    .render("index.html", &context)
                    .expect("Template should compile."),
            )
        });
        for (page_path, page_html) in pages {
            let page_path = self.build_dir().join(page_path);
            fs::create_dir_all(&page_path)?;
            let path = page_path.join("index.html");
            fs::write(&path, &page_html)?;
        }
        Ok(())
    }

    fn compile_rss<'a>(&self, path: &Path, posts: impl Iterator<Item = &'a Post>) -> Result<()> {
        let items: Vec<_> = posts
            .map(|post| {
                rss::ItemBuilder::default()
                    .link(Some(post.url.clone()))
                    .title(Some(post.title.clone()))
                    .content(Some(post.html.clone()))
                    .description(post.description.clone())
                    .pub_date(Some(
                        post.meta
                            .published_at
                            .assume_offset(
                                UtcOffset::from_whole_seconds(60 * self.config.timezone)
                                    .expect("Timezone offset should be valid"),
                            )
                            .format(&Rfc2822)
                            .expect("Pub date should format"),
                    ))
                    .category(rss::CategoryBuilder::default().name(&post.category).build())
                    .build()
            })
            .collect();

        let channel = rss::ChannelBuilder::default()
            .title(&self.config.name)
            .link(&self.config.url)
            .description(&self.config.desc)
            .items(items)
            .build();

        fs::write(path, channel.to_string())?;
        Ok(())
    }

    fn build_dir(&self) -> PathBuf {
        self.config.root.join(".build")
    }

    fn build_category(&self, slug: &str, posts: &[Post]) -> Result<()> {
        let build_dir = self.build_dir();
        let path = PathBuf::from(slug);
        self.build_index(path, posts)?;
        self.build_posts(posts)?;

        let rss_dir = build_dir.join("rss");
        let rss_name = format!("{}.xml", slug.replace('/', "."));
        let rss_posts = posts.iter().filter(|post| !post.meta.draft).take(20);
        self.compile_rss(&rss_dir.join(rss_name), rss_posts)?;
        Ok(())
    }

    fn build_posts(&self, posts: &[Post]) -> Result<()> {
        let build_dir = self.build_dir();
        for post in posts {
            let mut context = Context::new();
            context.insert("site", &self.config);
            context.insert("post", &post);
            context.insert("category", &post.category);
            context.insert(
                "current_url",
                &format!("{}/{}/{}", self.config.url, post.category, post.slug),
            );
            let html = self
                .templates
                .render("post.html", &context)
                .expect("Template should compile.");

            let path = build_dir.join(&post.url);
            fs::create_dir_all(&path)?;
            let path = path.join("index.html");
            fs::write(&path, &html)?;
        }
        Ok(())
    }

    fn build_static(&self, template: &str) -> Result<()> {
        let mut context = Context::new();
        context.insert("site", &self.config);
        context.insert("current_url", template);
        let html = self
            .templates
            .render(template, &context)
            .expect("Template should compile.");
        let path = self.build_dir().join(template);
        fs::write(&path, &html)?;
        Ok(())
    }

    pub fn build(&self) -> Result<()> {
        // let build_dir = self.config.root.join(".build");
        let build_dir = self.build_dir();
        fs::create_dir_all(&build_dir)?;
        clean_dir(&build_dir)?;

        let rss_dir = build_dir.join("rss");
        fs::create_dir_all(&rss_dir)?;

        let templates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("templates");
        symlink(
            &self.config.root.join(ASSETS_DIR),
            &build_dir.join(ASSETS_DIR),
        )?;
        symlink(
            &self.config.root.join("assets/favicon.ico"),
            &build_dir.join("favicon.ico"),
        )?;
        symlink(&templates_dir.join("css"), &build_dir.join("css"))?;
        self.build_static("404.html")?;

        let categories = find_categories(&self.config.root)?
            .into_iter()
            .map(|(cat_slug, md_paths)| {
                let mut posts = md_paths
                    .into_iter()
                    .map(|path| self.compile_post(&cat_slug, &path))
                    .collect::<Result<Vec<Post>>>()?;
                posts.sort_by_key(|post| Reverse(post.meta.published_at));
                Ok(Category {
                    slug: cat_slug,
                    posts,
                })
            })
            .collect::<Result<Vec<Category>>>()?;

        for category in &categories {
            self.build_category(&category.slug, &category.posts)?;
        }

        let mut posts: Vec<_> = categories
            .into_iter()
            .map(|cat| cat.posts)
            .flatten()
            .collect();
        posts.sort_by_key(|post| Reverse(post.meta.published_at));
        self.build_index(PathBuf::new(), &posts)?;

        let rss_posts = posts.iter().filter(|post| !post.meta.draft).take(20);
        self.compile_rss(&rss_dir.join("rss.xml"), rss_posts)?;

        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Config {
    /// Site root to search for posts.
    root: PathBuf,

    /// Name/title of the site.
    name: String,

    /// Canonical URL of the site.
    url: String,

    /// A brief description of the site.
    desc: String,

    /// Primary image for the site.
    image: String,

    /// External links to include.
    links: Vec<Link>,

    /// All published datetimes will be assumed
    /// to have this UTC offset.
    timezone: i32,

    /// How many posts to display per page.
    per_page: usize,
}

#[derive(Serialize, Deserialize)]
struct Link {
    url: String,
    name: String,
}

#[derive(Serialize)]
struct Post {
    url: String,
    slug: String,
    html: String,
    title: String,
    category: String,
    description: Option<String>,
    image: Option<String>,
    meta: FrontMatter,
}

#[derive(Debug, Serialize, Deserialize)]
struct FrontMatter {
    #[serde(default = "now", with = "serde_datetime")]
    published_at: PrimitiveDateTime,

    #[serde(default)]
    draft: bool,
}

#[derive(Serialize)]
struct Page<'a> {
    page: usize,
    next: Option<usize>,
    prev: Option<usize>,
    posts: &'a [&'a Post],
}

struct Category {
    slug: String,
    posts: Vec<Post>,
}

fn now() -> PrimitiveDateTime {
    let dt = OffsetDateTime::now_local().expect("Should be able to get local datetime");
    PrimitiveDateTime::new(dt.date(), dt.time())
}

/// Find all post category directories under a root path.
fn find_categories(root: &Path) -> Result<HashMap<String, Vec<PathBuf>>> {
    let mut categories: HashMap<String, Vec<PathBuf>> = HashMap::default();

    fn is_assets_dir(entry: &DirEntry) -> bool {
        entry.file_name() == ASSETS_DIR
    }

    let walker = WalkDir::new(root).into_iter();
    for entry in walker.filter_entry(|e| !is_assets_dir(e)) {
        let entry = entry?;
        let path = entry.path();
        let is_markdown = path.extension().is_some_and(|ext| ext == "md");
        if is_markdown {
            let rel_path = path
                .parent()
                .expect("Path should have parent")
                .strip_prefix(root)
                .expect("Path should be under root dir");
            let cat_slug = rel_path.to_string_lossy().to_string();
            let posts = categories.entry(cat_slug).or_default();
            posts.push(path.to_path_buf());
        }
    }
    Ok(categories)
}

/// Extract YAML front-matter from a string.
/// This returns the front matter and the rest of the string's contents.
fn extract_metadata(raw: &str) -> Result<(FrontMatter, String)> {
    let matter = Matter::<YAML>::new();
    let meta = matter
        .parse_with_struct::<FrontMatter>(raw)
        .expect("Front matter should always be present and valid");
    Ok((meta.data, meta.content))
}

/// Iterate over a collection of posts in pages.
fn paginate<'a>(posts: &'a [&'a Post], per_page: usize) -> impl Iterator<Item = Page<'a>> {
    assert!(per_page > 0, "`per_page` must be > 0.");

    let n_pages = posts.len().div_ceil(per_page);
    posts
        .chunks(per_page)
        .enumerate()
        .map(move |(i, posts)| Page {
            posts,
            page: i,
            prev: if i > 0 { Some(i) } else { None },
            next: if i < n_pages - 1 { Some(i + 2) } else { None },
        })
}

/// Create or update a symlink.
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    // Check if the symlink (or file/directory with the same name) already exists
    if link.exists() || link.symlink_metadata().is_ok() {
        // Attempt to remove the existing symlink (or file/directory)
        fs::remove_file(link)?;
    }

    // Create a new symlink
    symlink_dir(target, link)?;

    Ok(())
}

/// Remove all files and directories under the specified path.
fn clean_dir(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

/// `figure` image HTML
fn image_html(url: &str, title: &str, caption: &str) -> String {
    format!(
        "<figure>
            <a href=\"{url}\" title=\"{title}\">
            <img src=\"{url}\" title=\"{title}\">
            </a>
            <figcaption>{caption}</figcaption>
            </figure>"
    )
}

/// `figure` video HTML
fn video_html(url: &str, title: &str, caption: &str) -> String {
    format!(
        "<figure>
            <video autoplay loop muted src=\"{url}\" title=\"{title}\" />
            <figcaption>{caption}</figcaption>
            </figure>"
    )
}

struct Compiled {
    html: String,
    title: String,
    body: String,
    main_image: Option<String>,
}

/// Compile a markdown string to an html string,
/// while extracting some metadata and applying
/// custom processing.
fn compile_markdown(raw: &str) -> Result<Compiled> {
    use pulldown_cmark::{
        CodeBlockKind, Event, HeadingLevel,
        Tag::{CodeBlock, Heading, Image},
        TagEnd,
    };

    // To see if we're in a special processing context.
    enum Context {
        None,
        Title,
    }

    fn has_ext(url: &str, exts: &[&str]) -> bool {
        exts.iter().any(|ext| url.ends_with(ext))
    }

    fn is_image(url: &str) -> bool {
        has_ext(url, &[".png", ".jpg", ".jpeg", ".webp"])
    }

    fn is_video(url: &str) -> bool {
        has_ext(url, &[".mp4"])
    }

    let options = pulldown_cmark::Options::all();
    let mut parser = pulldown_cmark::Parser::new_ext(raw, options);
    let mut syn = inkjet::Highlighter::new();

    let mut context = Context::None;
    let mut body = String::new();
    let mut main_image = None;
    let mut events = vec![];
    let mut title_evs = vec![];

    while let Some(ev) = parser.next() {
        // If we want to replace an event with a custom one,
        // e.g. with custom HTML, set this to `Some`.
        let mut replacement_ev: Option<Event> = None;

        match &ev {
            // For extracting the post title, as an H1 header.
            Event::Start(Heading {
                level: HeadingLevel::H1,
                ..
            }) => {
                if title_evs.is_empty() {
                    context = Context::Title;
                }
            }
            Event::End(TagEnd::Heading(HeadingLevel::H1)) => {
                context = Context::None;
            }
            Event::Text(text) => match context {
                // We're ignoring the title since
                // we will handle it separately.
                Context::Title => (),
                _ => {
                    body.push_str(&text);
                }
            },
            Event::End(TagEnd::Paragraph) => {
                body.push_str("\n");
            }

            // Anything image-like we want to modify,
            // e.g. videos.
            Event::Start(Image {
                dest_url, title, ..
            }) => {
                if main_image.is_none() && is_image(dest_url) {
                    main_image = Some(dest_url.to_string());
                }

                let mut caption = String::new();
                while let Some(Event::Text(text)) = parser.next() {
                    caption.push_str(text.as_ref());
                }
                parser.next(); // consume the associated `Event::End`

                replacement_ev = Some(if is_video(dest_url) {
                    Event::Html(video_html(dest_url, &title, &caption).into())
                } else {
                    Event::Html(image_html(dest_url, &title, &caption).into())
                });
            }

            // Apply syntax highlighting to code blocks.
            Event::Start(CodeBlock(kind)) => match kind {
                CodeBlockKind::Fenced(lang) => {
                    if let Some(Event::Text(code)) = parser.next() {
                        parser.next(); // consume the associated `Event::End`
                        let lang = if lang.is_empty() {
                            inkjet::Language::Plaintext
                        } else {
                            inkjet::Language::from_token(lang).expect("Language should exist")
                        };
                        let html = syn.highlight_to_string(lang, &inkjet::formatter::Html, code)?;
                        let html = format!("<pre>{html}</pre>");
                        replacement_ev = Some(Event::Html(html.into()));
                    }
                }
                _ => (),
            },
            _ => (),
        }

        match context {
            // Drop the title header as we include it manually,
            // but we do keep track of these events separately so we
            // can preserve the HTML elements in the title.
            Context::Title => match ev {
                // Ignore the opening header tag.
                Event::Start(Heading {
                    level: HeadingLevel::H1,
                    ..
                }) => (),
                _ => {
                    title_evs.push(ev);
                }
            },
            _ => {
                events.push(replacement_ev.unwrap_or(ev));
            }
        }
    }

    let mut title = String::new();
    pulldown_cmark::html::push_html(&mut title, title_evs.into_iter());

    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events.into_iter());
    Ok(Compiled {
        html,
        title,
        body,
        main_image,
    })
}

fn main() {
    let port = Port {
        config: {
            let home = std::env::var("HOME").expect("HOME env var should be defined");
            let path = PathBuf::from(home).join(".config/port.yml");
            let file = fs::File::open(path).unwrap();
            serde_yaml::from_reader(file).unwrap()
        },
        templates: match Tera::new("templates/**/*.html") {
            Ok(t) => t,
            Err(e) => {
                println!("Parsing error(s): {}", e);
                ::std::process::exit(1);
            }
        },
    };
    println!("Building site \"{}\"", port.config.name);
    port.build().unwrap();
    println!("Done building.");
}
