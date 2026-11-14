//! B152: the grid pictures are prepared in the background. A pass writes each Primary image's grid
//! master and stores its BlurHash where it has none, skips what is ready, and counts what fails.

use std::path::{Path, PathBuf};

use rustyfin_core::image_proxy;
use rustyfin_core::image_warmer::{WarmReport, warm_pass};
use rustyfin_model::image::{ImageType, ItemImage};
use rustyfin_model::library::{Library, LibraryContentType};
use rustyfin_model::media::{MediaItem, MediaType};
use sqlx::SqlitePool;
use uuid::Uuid;

struct Fixture {
    pool: SqlitePool,
    dir: PathBuf,
    library_id: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("rustyfin_test_warm_{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("film")).unwrap();
        let pool = rustyfin_db::init_db(dir.join("test.db").to_str().unwrap())
            .await
            .unwrap();
        let library = Library {
            id: Uuid::new_v4(),
            name: "Film".to_string(),
            content_type: LibraryContentType::Movies,
            paths: vec![dir.join("film").to_string_lossy().into_owned()],
        };
        rustyfin_db::repo_library::create_library(&pool, &library)
            .await
            .unwrap();
        Self {
            pool,
            dir,
            library_id: library.id,
        }
    }

    /// A movie whose Primary image is the file `poster` (with `blurhash` stored, if given).
    async fn movie(&self, name: &str, poster: &Path, blurhash: Option<&str>) -> ItemImage {
        let item = MediaItem {
            id: Uuid::new_v4(),
            name: name.to_string(),
            sort_name: Some(name.to_lowercase()),
            media_type: MediaType::Movie,
            path: self
                .dir
                .join("film")
                .join(format!("{name}.mkv"))
                .to_string_lossy()
                .into_owned(),
            library_id: self.library_id,
            ..MediaItem::default_dto_fields()
        };
        rustyfin_db::repo_media::upsert_media_item(&self.pool, &item)
            .await
            .unwrap();
        let img = ItemImage {
            item_id: item.id,
            image_type: ImageType::Primary,
            image_index: 0,
            path: poster.to_string_lossy().into_owned(),
            provider: "local".to_string(),
            width: None,
            height: None,
            blurhash: blurhash.map(str::to_string),
            stamp: Some(42),
        };
        rustyfin_db::repo_image::upsert_image(&self.pool, &img)
            .await
            .unwrap();
        img
    }

    async fn stored_blurhash(&self, img: &ItemImage) -> Option<String> {
        rustyfin_db::repo_image::get_image(&self.pool, img.item_id, &ImageType::Primary, 0)
            .await
            .unwrap()
            .unwrap()
            .blurhash
    }

    fn master(&self, img: &ItemImage) -> PathBuf {
        self.dir
            .join("cache")
            .join("images")
            .join(image_proxy::grid_master_name(&img.path, img.stamp))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_jpeg(path: &Path, width: u32, height: u32) {
    image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    })
    .save(path)
    .unwrap();
}

#[tokio::test]
async fn b152_a_pass_prepares_masters_and_blurhashes_once() {
    let fx = Fixture::new().await;
    let big = fx.dir.join("film").join("big.jpg");
    write_jpeg(&big, 1000, 1500);
    let hashed_before = fx.dir.join("film").join("hashed.jpg");
    write_jpeg(&hashed_before, 600, 900);
    let broken = fx.dir.join("film").join("broken.jpg");
    std::fs::write(&broken, b"not a picture").unwrap();

    let a = fx.movie("A", &big, None).await;
    let b = fx
        .movie("B", &hashed_before, Some("L00000fQfQfQfQfQfQfQfQfQfQfQ"))
        .await;
    let c = fx.movie("C", &broken, None).await;
    let client = reqwest::Client::new();

    let report = warm_pass(&fx.pool, &fx.dir, &client).await.unwrap();
    assert_eq!(
        (report.prepared, report.hashed, report.ready, report.failed),
        (2, 1, 0, 1),
        "{report:?}"
    );
    assert!(!report.capped);

    // A's master fits the grid bounds exactly, and A got a BlurHash
    let master = image::open(fx.master(&a)).unwrap();
    assert_eq!((master.width(), master.height()), image_proxy::GRID_MASTER);
    let hash = fx
        .stored_blurhash(&a)
        .await
        .expect("A's BlurHash is stored");
    assert!(blurhash::decode(&hash, 4, 4, 1.0).is_ok(), "{hash}");
    // B kept the hash it had; the broken picture has neither
    assert_eq!(
        fx.stored_blurhash(&b).await.as_deref(),
        Some("L00000fQfQfQfQfQfQfQfQfQfQfQ")
    );
    assert!(fx.master(&b).exists());
    assert!(fx.stored_blurhash(&c).await.is_none());
    assert!(!fx.master(&c).exists());

    // A second pass finds both ready and writes nothing
    let again = warm_pass(&fx.pool, &fx.dir, &client).await.unwrap();
    assert_eq!(
        again,
        WarmReport {
            ready: 2,
            failed: 1,
            ..WarmReport::default()
        }
    );
}

#[tokio::test]
async fn b152_grid_copies_come_from_the_master_and_larger_ones_from_the_original() {
    let fx = Fixture::new().await;
    let poster = fx.dir.join("film").join("poster.jpg");
    write_jpeg(&poster, 1000, 1500);
    let img = fx.movie("A", &poster, None).await;
    warm_pass(&fx.pool, &fx.dir, &reqwest::Client::new())
        .await
        .unwrap();

    // With the original gone, a grid copy is still made, from the master...
    std::fs::write(&poster, b"gone").unwrap();
    let cache = fx.dir.join("cache").join("images");
    let copy = |width, height| {
        image_proxy::render_image(
            &img.path,
            img.stamp,
            image_proxy::Rendition::Copy { width, height },
            &cache,
            None,
        )
    };
    let small = image::load_from_memory(&copy(160, 240).await.unwrap()).unwrap();
    assert_eq!((small.width(), small.height()), (160, 240));
    // ...and the master itself is served as it is
    assert!(copy(480, 720).await.is_ok());
    // A copy larger than the master needs the original
    assert!(copy(640, 960).await.is_err());
}
