//! Canvas 2D through JSC: the classes, the HTML argument rules, and pixels
//! read back with `getImageData` — what a page actually observes.

use crate::api::Context;
use crate::glue;
use crate::webapi::testing::*;

unsafe fn pump_locked(c: &Context) {
    glue::rjs_lock(c.raw());
    crate::web::pump(c.raw(), crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS, core::ptr::null_mut());
    glue::rjs_unlock(c.raw());
}

/// An async body's settled value, stringified.
fn run(c: &Context, body: &str) -> String {
    s(
        c,
        &format!(
            "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
               v => {{ globalThis.__out = String(v); }}, \
               e => {{ globalThis.__out = 'ERROR: ' + (e && e.name) + ': ' + ((e && e.message) || e); }}); 'started'"
        ),
    );
    for _ in 0..200 {
        unsafe { pump_locked(c) };
        if s(c, "globalThis.__out") != "<pending>" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    s(c, "globalThis.__out")
}

/// `px(x, y)` reads one pixel of `globalThis.cx` as "r,g,b,a".
const PX: &str = "globalThis.px = (x, y) => Array.from(cx.getImageData(x, y, 1, 1).data).join(',');";

fn canvas(c: &Context, w: u32, h: u32) {
    s(c, &format!("globalThis.oc = new OffscreenCanvas({w}, {h}); globalThis.cx = oc.getContext('2d'); {PX} 0"));
}

#[test]
fn the_classes_exist_tagged_and_the_contexts_are_not_constructible() {
    let c = ctx();
    for name in [
        "CanvasRenderingContext2D",
        "OffscreenCanvasRenderingContext2D",
        "OffscreenCanvas",
        "CanvasGradient",
        "CanvasPattern",
        "TextMetrics",
    ] {
        ok(&c, &format!("typeof {name} === 'function' && Object.prototype.toString.call({name}.prototype) === '[object {name}]'"));
    }
    ok(&c, "new OffscreenCanvas(1, 1) instanceof EventTarget");
    ok(&c, "/native code/.test(OffscreenCanvasRenderingContext2D.prototype.fillRect.toString())");
    assert!(throws(&c, "new CanvasRenderingContext2D()").contains("Illegal constructor"));
    assert!(throws(&c, "new OffscreenCanvas(-1, 1)").contains("unsigned long long"));
}

#[test]
fn get_context_is_one_object_per_canvas_and_other_kinds_are_null() {
    let c = ctx();
    canvas(&c, 4, 4);
    ok(&c, "oc.getContext('2d') === cx && cx.canvas === oc");
    ok(&c, "cx instanceof OffscreenCanvasRenderingContext2D");
    ok(&c, "new OffscreenCanvas(1, 1).getContext('webgl') === null");
    assert!(throws(&c, "oc.getContext('3d')").contains("OffscreenRenderingContextId"));
    ok(&c, "cx.getContextAttributes().alpha === true && cx.getContextAttributes().willReadFrequently === false");
}

#[test]
fn styles_serialize_as_html_has_them_and_junk_is_ignored() {
    let c = ctx();
    canvas(&c, 1, 1);
    ok(&c, "cx.fillStyle === '#000000'");
    ok(&c, "(cx.fillStyle = 'RED', cx.fillStyle) === '#ff0000'");
    ok(&c, "(cx.fillStyle = 'rgba(0, 0, 255, 0.5)', cx.fillStyle) === 'rgba(0, 0, 255, 0.5)'");
    ok(&c, "(cx.fillStyle = 'not a colour', cx.fillStyle) === 'rgba(0, 0, 255, 0.5)'");
    ok(&c, "(cx.strokeStyle = 'oklch(1 0 0)', cx.strokeStyle) === '#ffffff'");
    ok(&c, "(cx.globalAlpha = 2, cx.globalAlpha) === 1 && (cx.globalAlpha = 0.25, cx.globalAlpha) === 0.25");
    ok(&c, "(cx.globalCompositeOperation = 'Copy', cx.globalCompositeOperation) === 'source-over'");
    ok(&c, "(cx.globalCompositeOperation = 'source-atop', cx.globalCompositeOperation) === 'source-atop'");
    ok(&c, "(cx.lineCap = 'round', cx.lineCap) === 'round' && (cx.lineCap = 'nope', cx.lineCap) === 'round'");
    ok(&c, "(cx.lineWidth = -1, cx.lineWidth) === 1 && (cx.lineWidth = 3, cx.lineWidth) === 3");
    ok(&c, "(cx.setLineDash([1, 2, 3]), cx.getLineDash().join()) === '1,2,3,1,2,3'");
    ok(&c, "(cx.font = 'bold 12px Arial', cx.font) === 'bold 12px Arial' && (cx.font = 'Arial', cx.font) === 'bold 12px Arial'");
    let g = "const g = cx.createLinearGradient(0, 0, 1, 0); cx.fillStyle = g; return cx.fillStyle === g";
    ok(&c, &format!("(() => {{ {g} }})()"));
}

#[test]
fn save_and_restore_carry_every_style_including_a_gradient_object() {
    let c = ctx();
    canvas(&c, 1, 1);
    ok(
        &c,
        "(() => { const g = cx.createRadialGradient(0, 0, 0, 0, 0, 1); cx.fillStyle = g; cx.save(); \
         cx.fillStyle = 'red'; cx.lineWidth = 7; cx.translate(5, 0); cx.save(); cx.restore(); \
         const inner = cx.fillStyle === '#ff0000' && cx.getTransform().e === 5; cx.restore(); \
         return inner && cx.fillStyle === g && cx.lineWidth === 1 && cx.getTransform().e === 0; })()",
    );
}

#[test]
fn fill_rect_and_get_image_data_see_the_pixels() {
    let c = ctx();
    canvas(&c, 4, 4);
    s(&c, "cx.fillStyle = 'rgb(10, 20, 30)'; cx.fillRect(1, 1, 2, 2); 0");
    assert_eq!(s(&c, "px(1, 1)"), "10,20,30,255");
    assert_eq!(s(&c, "px(0, 0)"), "0,0,0,0");
    assert_eq!(s(&c, "px(3, 3)"), "0,0,0,0");
    // Reading outside the bitmap is transparent black, not an error.
    ok(&c, "cx.getImageData(-2, -2, 4, 4).data.length === 64");
    assert!(throws(&c, "cx.getImageData(0, 0, 0, 1)").contains("non-zero"));
    ok(&c, "cx.getImageData(0, 0, 1, 1) instanceof ImageData");
}

#[test]
fn source_atop_tints_only_what_is_drawn_as_the_client_does() {
    let c = ctx();
    canvas(&c, 4, 1);
    s(
        &c,
        "cx.fillStyle = '#fff'; cx.fillRect(0, 0, 2, 1); \
         cx.globalCompositeOperation = 'source-atop'; cx.fillStyle = 'rgb(255, 0, 0)'; cx.fillRect(0, 0, 4, 1); \
         cx.globalCompositeOperation = 'source-over'; 0",
    );
    assert_eq!(s(&c, "px(0, 0)"), "255,0,0,255");
    assert_eq!(s(&c, "px(3, 0)"), "0,0,0,0");
}

#[test]
fn clear_rect_then_draw_is_the_animation_loop() {
    let c = ctx();
    canvas(&c, 4, 4);
    for i in 0..50 {
        s(&c, &format!("cx.clearRect(0, 0, 4, 4); cx.fillStyle = 'rgb({i}, 0, 0)'; cx.fillRect(0, 0, 4, 4); 0"));
    }
    assert_eq!(s(&c, "px(2, 2)"), "49,0,0,255");
    s(&c, "cx.clearRect(1, 1, 1, 1); 0");
    assert_eq!(s(&c, "px(1, 1)"), "0,0,0,0");
    assert_eq!(s(&c, "px(2, 2)"), "49,0,0,255");
}

#[test]
fn draw_image_scales_crops_and_takes_a_canvas() {
    let c = ctx();
    canvas(&c, 8, 8);
    // A 2x2 source: red, green / blue, white.
    s(
        &c,
        "globalThis.src = new OffscreenCanvas(2, 2); const sx = src.getContext('2d'); \
         sx.putImageData(new ImageData(new Uint8ClampedArray([255,0,0,255, 0,255,0,255, 0,0,255,255, 255,255,255,255]), 2, 2), 0, 0); \
         cx.imageSmoothingEnabled = false; cx.drawImage(src, 0, 0, 8, 8); 0",
    );
    assert_eq!(s(&c, "px(1, 1)"), "255,0,0,255");
    assert_eq!(s(&c, "px(6, 1)"), "0,255,0,255");
    assert_eq!(s(&c, "px(1, 6)"), "0,0,255,255");
    assert_eq!(s(&c, "px(6, 6)"), "255,255,255,255");
    // 9 arguments: only the white pixel, into a 2x2 corner.
    s(&c, "cx.clearRect(0, 0, 8, 8); cx.drawImage(src, 1, 1, 1, 1, 0, 0, 2, 2); 0");
    assert_eq!(s(&c, "px(1, 1)"), "255,255,255,255");
    assert_eq!(s(&c, "px(3, 3)"), "0,0,0,0");
    assert!(throws(&c, "cx.drawImage(src, 0, 0, 1)").contains("overload"));
    assert!(throws(&c, "cx.drawImage({}, 0, 0)").contains("is not of type"));
}

#[test]
fn put_image_data_ignores_transform_and_honours_the_dirty_rect() {
    let c = ctx();
    canvas(&c, 4, 4);
    s(
        &c,
        "cx.translate(2, 2); cx.globalAlpha = 0.1; \
         const d = new ImageData(new Uint8ClampedArray([1,2,3,255, 4,5,6,255]), 2, 1); \
         cx.putImageData(d, 0, 0); cx.putImageData(d, 0, 2, 1, 0, 1, 1); 0",
    );
    assert_eq!(s(&c, "px(0, 0)"), "1,2,3,255");
    assert_eq!(s(&c, "px(1, 0)"), "4,5,6,255");
    assert_eq!(s(&c, "px(1, 2)"), "4,5,6,255");
    assert_eq!(s(&c, "px(0, 2)"), "0,0,0,0");
}

#[test]
fn paths_follow_the_transform_they_were_built_under() {
    let c = ctx();
    canvas(&c, 20, 20);
    s(&c, "cx.fillStyle = '#00f'; cx.translate(10, 10); cx.beginPath(); cx.arc(0, 0, 5, 0, Math.PI * 2); cx.resetTransform(); cx.fill(); 0");
    assert_eq!(s(&c, "px(10, 10)"), "0,0,255,255");
    assert_eq!(s(&c, "px(2, 2)"), "0,0,0,0");
    ok(&c, "cx.isPointInPath(10, 10) && !cx.isPointInPath(1, 1)");
    s(&c, "cx.beginPath(); cx.rect(0, 0, 4, 4); cx.lineWidth = 2; 0");
    ok(&c, "cx.isPointInStroke(0, 2) && !cx.isPointInStroke(2, 2)");
}

#[test]
fn a_clip_bounds_later_draws_until_restore() {
    let c = ctx();
    canvas(&c, 10, 10);
    s(
        &c,
        "cx.save(); cx.beginPath(); cx.rect(0, 0, 5, 10); cx.clip(); cx.fillStyle = '#0f0'; cx.fillRect(0, 0, 10, 10); \
         cx.restore(); cx.fillStyle = '#f00'; cx.fillRect(8, 8, 2, 2); 0",
    );
    assert_eq!(s(&c, "px(2, 2)"), "0,255,0,255");
    assert_eq!(s(&c, "px(7, 2)"), "0,0,0,0");
    assert_eq!(s(&c, "px(9, 9)"), "255,0,0,255");
}

#[test]
fn gradients_paint_and_their_stops_are_checked() {
    let c = ctx();
    canvas(&c, 100, 1);
    s(
        &c,
        "const g = cx.createLinearGradient(0, 0, 100, 0); g.addColorStop(0, '#f00'); g.addColorStop(1, '#00f'); \
         cx.fillStyle = g; cx.fillRect(0, 0, 100, 1); 0",
    );
    ok(&c, "cx.getImageData(1, 0, 1, 1).data[0] > 240 && cx.getImageData(98, 0, 1, 1).data[2] > 240");
    assert!(throws(&c, "cx.createLinearGradient(0, 0, 1, 0).addColorStop(2, 'red')").contains("outside"));
    assert!(throws(&c, "cx.createLinearGradient(0, 0, 1, 0).addColorStop(0, 'nope')").contains("could not be parsed"));
    ok(&c, "(() => { try { cx.createLinearGradient(0,0,1,0).addColorStop(0, 'nope'); } catch (e) { return e instanceof DOMException && e.name === 'SyntaxError'; } })()");
}

#[test]
fn width_resets_the_bitmap_and_the_state() {
    let c = ctx();
    canvas(&c, 4, 4);
    s(&c, "cx.fillStyle = 'red'; cx.fillRect(0, 0, 4, 4); cx.translate(1, 1); oc.width = 6; 0");
    assert_eq!(s(&c, "px(0, 0)"), "0,0,0,0");
    ok(&c, "cx.fillStyle === '#000000' && cx.getTransform().e === 0 && oc.width === 6");
    ok(&c, "cx.getImageData(5, 0, 1, 1).data.length === 4");
}

#[test]
fn convert_to_blob_and_transfer_to_image_bitmap() {
    let c = ctx();
    canvas(&c, 3, 2);
    let got = run(
        &c,
        "cx.fillStyle = '#0f0'; cx.fillRect(0, 0, 3, 2); \
         const png = await oc.convertToBlob(); const jpg = await oc.convertToBlob({ type: 'image/jpeg', quality: 0.5 }); \
         const bmp = oc.transferToImageBitmap(); \
         return [png.type, png.size > 20, jpg.type, bmp.width, bmp.height, px(1, 1)].join();",
    );
    assert_eq!(got, "image/png,true,image/jpeg,3,2,0,0,0,0");
    let got = run(&c, "await new OffscreenCanvas(0, 1).convertToBlob()");
    assert!(got.contains("IndexSizeError"), "{got}");
}

#[test]
fn create_image_bitmap_takes_an_offscreen_canvas() {
    let c = ctx();
    canvas(&c, 2, 2);
    let got = run(&c, "cx.fillStyle = '#123456'; cx.fillRect(0, 0, 2, 2); const b = await createImageBitmap(oc); return b.width + 'x' + b.height;");
    assert_eq!(got, "2x2");
}

#[test]
fn an_embedder_element_gets_a_context_its_pixels_and_a_source_hook() {
    let c = ctx();
    s(
        &c,
        "globalThis.el = { tagName: 'CANVAS' }; globalThis.cx = __rjs_canvas_context(el, 4, 4, { willReadFrequently: true }, 0); \
         globalThis.px = (x, y) => Array.from(cx.getImageData(x, y, 1, 1).data).join(','); 0",
    );
    ok(&c, "cx instanceof CanvasRenderingContext2D && cx.canvas === el && typeof cx.drawFocusIfNeeded === 'function'");
    ok(&c, "__rjs_canvas_info(cx).backend === 'vello_cpu' && __rjs_canvas_info(cx).width === 4");
    ok(&c, "__rjs_canvas_data_url(null, 0, 3, 'image/png') === 'data:,'");
    s(&c, "cx.fillStyle = '#f00'; cx.fillRect(0, 0, 4, 4); 0");
    ok(&c, "__rjs_canvas_data_url(cx, 4, 4, 'image/png').startsWith('data:image/png;base64,')");
    ok(&c, "__rjs_canvas_blob(cx, 4, 4, 'image/webp') instanceof Blob && __rjs_canvas_blob(null, 0, 0, '') === null");
    // drawImage(element) goes through the hook; null means "nothing to draw yet".
    s(&c, "__rjs_canvas_source_hook(v => v === el ? cx : v && v.loading ? null : undefined); 0");
    s(&c, "globalThis.oc = new OffscreenCanvas(4, 4); globalThis.o2 = oc.getContext('2d'); o2.drawImage(el, 0, 0); o2.drawImage({ loading: true }, 0, 0); 0");
    ok(&c, "o2.getImageData(2, 2, 1, 1).data[0] === 255");
    assert!(throws(&c, "o2.drawImage({}, 0, 0)").contains("is not of type"));
    s(&c, "__rjs_canvas_resize(cx, 2, 2); 0");
    assert_eq!(s(&c, "px(0, 0)"), "0,0,0,0");
}

/// The embedder's own `<img>`: its hook hands back an ImageBitmap of the
/// decoded pixels, which drawImage and createImageBitmap both take, and a
/// cross-origin one takes the canvas's origin-clean flag with it.
#[test]
fn an_embedder_image_is_a_bitmap_from_the_hook_and_carries_its_origin() {
    use crate::webapi::image::{create_bitmap, BitmapData};
    let c = ctx();
    unsafe {
        let g = c.raw();
        glue::rjs_lock(g);
        let near = create_bitmap(g, BitmapData::new(1, 1, vec![0, 0, 255, 255]));
        crate::webapi::util::define(g, glue::rjs_global(g), "nearBmp", near, 0);
        let mut far = BitmapData::new(1, 1, vec![0, 255, 0, 255]);
        far.origin_clean = false;
        let far = create_bitmap(g, far);
        crate::webapi::util::define(g, glue::rjs_global(g), "farBmp", far, 0);
        glue::rjs_unlock(g);
    }
    s(
        &c,
        "globalThis.near = { tagName: 'IMG' }; globalThis.far = { tagName: 'IMG' }; \
         __rjs_canvas_source_hook(v => v === near ? nearBmp : v === far ? farBmp : undefined); 0",
    );
    canvas(&c, 2, 1);
    s(&c, "cx.drawImage(near, 0, 0); 0");
    assert_eq!(s(&c, "px(0, 0)"), "0,0,255,255");
    ok(&c, "__rjs_canvas_info(cx).originClean === true");
    // Recorded, not enforced: the readback goes ahead (with an advisory).
    s(&c, "cx.drawImage(far, 1, 0); 0");
    assert_eq!(s(&c, "px(1, 0)"), "0,255,0,255");
    ok(&c, "__rjs_canvas_info(cx).originClean === false");
    assert_eq!(run(&c, "const b = await createImageBitmap(near); return b.width + 'x' + b.height;"), "1x1");
    let r = run(&c, "await createImageBitmap({ tagName: 'IMG' });");
    assert!(r.starts_with("ERROR:"), "{r}");
}

#[test]
fn text_measures_and_draws_nothing_without_throwing() {
    let c = ctx();
    canvas(&c, 10, 10);
    ok(&c, "(cx.font = '20px sans-serif', cx.measureText('hello')) instanceof TextMetrics && cx.measureText('hello').width > 20");
    s(&c, "cx.fillText('hi', 0, 10); cx.strokeText('hi', 0, 10); 0");
    assert_eq!(s(&c, "px(1, 9)"), "0,0,0,0");
}
