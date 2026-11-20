// SPDX-License-Identifier: MIT
use lunatui_core::{ClusterTable, DecodedGlyph, Grid, Rect};
use lunatui_image::{ImageId, ImageSource};
use lunatui_theme::{Theme, Variant};
use lunatui_widgets::{Image, RenderCx, Widget};
use native_client_probe::images::Images;

#[test]
fn actual_widget_fallback_survives_pending_and_decline_and_disappears_only_after_ready() {
    let source = ImageSource::rgba8(
        1,
        2,
        vec![255, 0, 0, 255, 0, 0, 255, 255].into_boxed_slice(),
    )
    .unwrap();
    let image = Image::new(source).id(ImageId::new(1).unwrap());
    let theme = Theme::builtin(Variant::Dark);
    let mut images = Images::new([8, 16]);
    let render = |images: &mut Images| {
        images.begin_frame();
        let mut grid = Grid::new(1, 1);
        let mut clusters = ClusterTable::new();
        image.render(
            Rect::new(0, 0, 1, 1),
            &mut RenderCx::new(&mut grid, &mut clusters, &theme).image_sink(images),
        );
        grid.get(0, 0).unwrap()
    };
    assert_eq!(
        render(&mut images).glyph().decode(),
        DecodedGlyph::Inline('▀')
    );
    assert!(images.placements().is_empty());
    let mut pending = [0; 48];
    for (index, value) in [(0, 48), (4, 11), (8, 1), (16, 4), (24, 7), (32, 17)] {
        pending[index] = value;
    }
    images.admission((1, 0), &pending).unwrap();
    assert_eq!(
        render(&mut images).glyph().decode(),
        DecodedGlyph::Inline('▀')
    );
    assert!(images.placements().is_empty());
    let mut ready = pending;
    ready[4] = 15;
    ready[16] = 5;
    ready[40] = 2;
    assert!(images.pong(&ready).unwrap());
    assert_eq!(
        render(&mut images).glyph().decode(),
        DecodedGlyph::Inline(' ')
    );
    assert_eq!(images.placements().len(), 1);
    let mut images = Images::new([8, 16]);
    let mut declined = pending;
    declined[32] = 0;
    declined[40] = 2;
    images.admission((1, 0), &declined).unwrap();
    assert_eq!(
        render(&mut images).glyph().decode(),
        DecodedGlyph::Inline('▀')
    );
    assert!(images.placements().is_empty());
    assert!(
        images.take_request().is_none(),
        "a decline must not enqueue the same source each frame"
    );
}
