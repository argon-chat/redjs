//! GPUCanvasContext through real JSC, native GPU work and canvas snapshots.
use super::*;
use crate::webapi::webgpu::canvas;
use crate::webapi::webgpu::{handle, Kind, Resource};
use std::sync::atomic::Ordering;

const HELPERS: &str = r#"
  globalThis.paintCanvas = (context, color, device = d) => {
    const encoder = device.createCommandEncoder();
    const pass = encoder.beginRenderPass({colorAttachments:[{
      view:context.getCurrentTexture().createView(), clearValue:color, loadOp:'clear', storeOp:'store'
    }]});
    pass.end(); device.queue.submit([encoder.finish()]);
  };
  globalThis.canvasPixel = source => {
    const copy = new OffscreenCanvas(1,1);
    const context = copy.getContext('2d');
    context.drawImage(source,0,0);
    return Array.from(context.getImageData(0,0,1,1).data).join(',');
  };
"#;
fn run_canvas(c: &Context, body: &str) -> String {
    run_gpu(c, &format!("{HELPERS}\n{body}"))
}
fn frame(
    c: &Context,
    key: u64,
    since: u64,
    size: (u32, u32),
) -> (u64, Arc<canvas::PresentedFrame>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        unsafe { pump(c) };
        if let Some((generation, frame)) = canvas::present(key, since) {
            if (frame.width, frame.height) == size
                && frame.rgba.len() == size.0 as usize * size.1 as usize * 4
            {
                return (generation, frame);
            }
        }
        assert!(
            Instant::now() < deadline,
            "Canvas {key} did not present a {size:?} frame"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn texture_destroyed(c: &Context, expression: &str) -> bool {
    let value = c
        .eval(expression)
        .expect("Texture expression must evaluate");
    let h = unsafe { handle(c.raw(), value.0, Kind::Texture) }.expect("Expected GPUTexture");
    let Resource::Texture(texture) = h.resource else {
        unreachable!()
    };
    texture.destroyed.load(Ordering::Acquire)
}
fn pump_frame(c: &Context) {
    unsafe {
        crate::glue::rjs_lock(c.raw());
        crate::ffi::redjs_pump(
            c.raw(),
            crate::web::PUMP_TIMERS
                | crate::web::PUMP_RAF
                | crate::web::PUMP_MICROTASKS
                | crate::web::PUMP_CHECKPOINT_PER_TASK,
            core::ptr::null_mut(),
        );
        crate::glue::rjs_unlock(c.raw());
    }
}

#[test]
fn webgpu_canvas_native_identity_and_context_modes() {
    let _host = HostGuard::new();
    let c = ctx();
    ok(
        &c,
        r#"(() => {
      const source = new OffscreenCanvas(2,2);
      if(source.getContext('webgl')!==null) return false;
      const gpu = source.getContext('webgpu');
      if(!(gpu instanceof GPUCanvasContext)||gpu.canvas!==source||source.getContext('webgpu')!==gpu||source.getContext('2d')!==null) return false;
      const other = new OffscreenCanvas(2,2); const two = other.getContext('2d');
      if(other.getContext('webgpu')!==null||other.getContext('2d')!==two) return false;
      const owner={}; const hidden=__rjs_webgpu_context(owner,2,2);
      return hidden.canvas===owner && hidden.getConfiguration()===null && gpu.getConfiguration()===null;
    })()"#,
    );
    assert!(throws(&c, "new GPUCanvasContext()").contains("Illegal constructor"));
    assert!(throws(
        &c,
        "new OffscreenCanvas(2,2).getContext('webgpu').getCurrentTexture()"
    )
    .contains("not configured"));
}

#[test]
fn webgpu_canvas_selection_survives_reentrant_and_throwing_2d_options() {
    let _host = HostGuard::new();
    let c = ctx();
    ok(&c, r#"(() => {
      const canvas=new OffscreenCanvas(2,2);let blocked=false;
      const context=canvas.getContext('2d',{
        get alpha(){
          blocked=canvas.getContext('webgpu')===null && canvas.getContext('2d')===null;
          canvas.width=4;canvas.height=3;return 'truthy';
        },
        willReadFrequently:0
      });
      if(!blocked||!context.getContextAttributes().alpha||context.getContextAttributes().willReadFrequently) return false;
      if(__rjs_sourcePixels(canvas).length!==4*3*4) return false;
      const retry=new OffscreenCanvas(1,1);const marker=new Error('options');let threw=false;
      try{retry.getContext('2d',{get alpha(){throw marker;}});}catch(e){threw=e===marker;}
      return threw && retry.getContext('webgpu') instanceof GPUCanvasContext;
    })()"#);
}

#[test]
fn webgpu_canvas_configuration_is_a_snapshot_and_preserves_device_identity() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_canvas(
            &c,
            r#"
      const source = new OffscreenCanvas(2,2); const gpu=source.getContext('webgpu');
      const formats=['rgba8unorm-srgb'];
      const options={device:d,format:'rgba8unorm',viewFormats:formats}; gpu.configure(options);
      formats.push('bgra8unorm'); options.format='bgra8unorm';
      const config=gpu.getConfiguration();
      if(config.device!==d||config.format!=='rgba8unorm'||config.usage!==GPUTextureUsage.RENDER_ATTACHMENT||
         config.alphaMode!=='opaque'||config.colorSpace!=='srgb'||config.toneMapping.mode!=='standard'||config.viewFormats.join(',')!=='rgba8unorm-srgb') throw new Error('Incorrect configuration snapshot');
      config.viewFormats[0]='bgra8unorm'; config.format='bgra8unorm'; config.toneMapping.mode='extended';
      const next=gpu.getConfiguration();
      if(next===config||next.format!=='rgba8unorm'||next.viewFormats.join(',')!=='rgba8unorm-srgb'||next.toneMapping.mode!=='standard') throw new Error('Configuration object aliases native state');
      let refused=0;
      for(const bad of [{format:'rgba16float'},{colorSpace:'display-p3'}]) {
        try {gpu.configure({device:d,format:'rgba8unorm',...bad});} catch(e) {if(e.name==='NotSupportedError') refused++;else throw e;}
      }
      const result=refused===2&&gpu.getConfiguration().format==='rgba8unorm'; gpu.unconfigure();d.destroy();return result;
    "#
        ),
        "true"
    );
}

#[test]
fn webgpu_canvas_render_snapshot_reaches_image_bitmap_2d_and_png() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_canvas(
            &c,
            r#"
      const source=new OffscreenCanvas(3,2);const gpu=source.getContext('webgpu');
      gpu.configure({device:d,format:'rgba8unorm'});paintCanvas(gpu,[0,1,0,1]);
      if(canvasPixel(source)!=='0,255,0,255') throw new Error('GPU canvas did not draw into 2D');
      const bitmap=await createImageBitmap(source);
      if(canvasPixel(bitmap)!=='0,255,0,255') throw new Error('GPU image bitmap pixels are wrong');
      const blob=await source.convertToBlob({type:'image/png'});
      if(blob.type!=='image/png') throw new Error('Incorrect PNG mime');
      const bytes=new Uint8Array(await blob.arrayBuffer());
      if(Array.from(bytes.slice(0,8)).join(',')!=='137,80,78,71,13,10,26,10') throw new Error('Not PNG data');
      const decoded=await createImageBitmap(blob);const result=canvasPixel(decoded);
      bitmap.close();decoded.close();gpu.unconfigure();d.destroy();return result;
    "#
        ),
        "0,255,0,255"
    );
}

#[test]
fn webgpu_canvas_alpha_modes_and_bgra_swizzle_have_correct_pixels() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_canvas(
            &c,
            r#"
      const answers=[];
      for(const format of ['rgba8unorm','bgra8unorm']) for(const alphaMode of ['opaque','premultiplied']) {
        const source=new OffscreenCanvas(1,1);const gpu=source.getContext('webgpu');
        gpu.configure({device:d,format,alphaMode});// Choose exact UNORM values, avoiding driver-specific half-LSB ties.
        paintCanvas(gpu,[64/255,32/255,0,128/255]);
        const exact=Array.from(__rjs_sourcePixels(source));
        const raster=canvasPixel(source).split(',').map(Number);
        // vello_cpu's 2D conversion can differ by one UNORM step. The GPU
        // snapshot itself is checked exactly, independently of that rasterizer.
        if(raster.some((v,i)=>Math.abs(v-exact[i])>1)) throw new Error('2D conversion changed GPU canvas color');
        answers.push(exact.join(','));gpu.unconfigure();
      }
      d.destroy();return answers.join('|');
    "#
        ),
        "64,32,0,255|128,64,0,128|64,32,0,255|128,64,0,128"
    );
}

#[test]
fn webgpu_canvas_hidden_element_exports_the_gpu_image() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_canvas(
            &c,
            r#"
      const owner={};const gpu=__rjs_webgpu_context(owner,2,1);
      gpu.configure({device:d,format:'bgra8unorm'});paintCanvas(gpu,[1,0,0,1]);
      const url=__rjs_webgpu_data_url(gpu,'image/png');
      if(!url.startsWith('data:image/png;base64,')) throw new Error('GPU URL was not encoded');
      const blob=__rjs_webgpu_blob(gpu,'image/png');const image=await createImageBitmap(blob);
      const result=canvasPixel(image);image.close();
      __rjs_webgpu_resize(gpu,0,0);
      if(__rjs_webgpu_data_url(gpu,'image/png')!=='data:,'||__rjs_webgpu_blob(gpu,'image/png')!==null) throw new Error('Zero-size element exports were not empty');
      const zero=new OffscreenCanvas(0,1);const z=zero.getContext('webgpu');z.configure({device:d,format:'rgba8unorm'});
      let refused=false;try{z.getCurrentTexture();}catch(e){refused=e.name==='OperationError';}
      let empty=false;try{await zero.convertToBlob();}catch(e){empty=e.name==='IndexSizeError';}
      if(!refused||!empty) throw new Error('Zero-size OffscreenCanvas was accepted');
      z.unconfigure();gpu.unconfigure();d.destroy();return result;
    "#
        ),
        "255,0,0,255"
    );
}

#[test]
fn webgpu_canvas_presents_only_completed_native_frames_with_generations() {
    let _host = HostGuard::new();
    let c = ctx();
    let key = 9_001_001;
    let _ = canvas::take_presented_changed();
    assert_eq!(
        run_canvas(
            &c,
            r#"
      globalThis.gpu=__rjs_webgpu_context({},2,1,9001001);
      gpu.configure({device:d,format:'bgra8unorm'});paintCanvas(gpu,[1,0,0,1]);return true;
    "#
        ),
        "true"
    );
    let (generation, image) = frame(&c, key, 0, (2, 1));
    assert_eq!(image.rgba, vec![255, 0, 0, 255, 255, 0, 0, 255]);
    assert_eq!(canvas::present_size(key), Some((2, 1)));
    assert!(canvas::present(key, generation).is_none());
    assert!(canvas::take_presented_changed());
    assert!(!canvas::take_presented_changed());
    canvas::unregister_present(key);
    ok(&c, "gpu.unconfigure();d.destroy();true");
}

#[test]
fn webgpu_canvas_resize_discards_old_frame_completion_and_texture() {
    let _host = HostGuard::new();
    let c = ctx();
    let key = 9_001_002;
    assert_eq!(
        run_canvas(
            &c,
            r#"
      globalThis.gpu=__rjs_webgpu_context({},4,4,9001002);gpu.configure({device:d,format:'rgba8unorm'});
      globalThis.oldTexture=gpu.getCurrentTexture();paintCanvas(gpu,[1,0,0,1]);return true;
    "#
        ),
        "true"
    );
    ok(&c, "__rjs_webgpu_resize(gpu,1,2);true");
    assert!(texture_destroyed(&c, "oldTexture"));
    assert_eq!(canvas::present_size(key), Some((1, 2)));
    let (reset, image) = canvas::present(key, 0).expect("Resize publishes a cleared generation");
    assert!(image.rgba.is_empty());
    for _ in 0..12 {
        unsafe { pump(&c) };
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        canvas::present(key, reset).is_none(),
        "Old readback completion republished after resize"
    );
    ok(&c,"globalThis.resizedTexture=gpu.getCurrentTexture();resizedTexture!==oldTexture&&resizedTexture.width===1&&resizedTexture.height===2");
    ok(&c,"(()=>{try{oldTexture.createView();return false;}catch(e){return e instanceof TypeError;}})()");
    ok(&c, "paintCanvas(gpu,[0,1,0,1]);true");
    let (_, image) = frame(&c, key, reset, (1, 2));
    assert_eq!(image.rgba, vec![0, 255, 0, 255, 0, 255, 0, 255]);
    canvas::unregister_present(key);
    ok(&c, "gpu.unconfigure();d.destroy();true");
}

#[test]
fn webgpu_canvas_unconfigure_cancels_frames_and_can_rebind_another_device() {
    let _host = HostGuard::new();
    let c = ctx();
    let key = 9_001_003;
    assert_eq!(
        run_canvas(
            &c,
            r#"
      globalThis.gpu=__rjs_webgpu_context({},2,1,9001003);gpu.configure({device:d,format:'rgba8unorm'});
      globalThis.oldTexture=gpu.getCurrentTexture();paintCanvas(gpu,[1,0,0,1]);return true;
    "#
        ),
        "true"
    );
    ok(&c, "gpu.unconfigure();gpu.getConfiguration()===null");
    assert!(texture_destroyed(&c, "oldTexture"));
    assert!(throws(&c, "gpu.getCurrentTexture()").contains("not configured"));
    let (generation, blank) = canvas::present(key, 0).expect("Unconfigure clears the image");
    assert!(blank.rgba.is_empty());
    for _ in 0..12 {
        unsafe { pump(&c) };
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(canvas::present(key, generation).is_none());
    assert_eq!(
        run(
            &c,
            r#"
      globalThis.d2=await adapter.requestDevice();gpu.configure({device:d2,format:'bgra8unorm'});
      paintCanvas(gpu,[0,0,1,1],d2);d.destroy();return canvasPixel(gpu);
    "#
        ),
        "0,0,255,255"
    );
    let (_, image) = frame(&c, key, generation, (2, 1));
    assert_eq!(image.rgba, vec![0, 0, 255, 255, 0, 0, 255, 255]);
    canvas::unregister_present(key);
    ok(&c, "gpu.unconfigure();d2.destroy();true");
}
#[test]
fn webgpu_canvas_explicit_texture_destroy_keeps_image_and_rejects_old_views() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_canvas(
            &c,
            r#"
      const source=new OffscreenCanvas(1,1);const gpu=source.getContext('webgpu');
      gpu.configure({device:d,format:'rgba8unorm'});paintCanvas(gpu,[0,1,0,1]);
      const texture=gpu.getCurrentTexture();const oldView=texture.createView();texture.destroy();
      if(gpu.getCurrentTexture()!==texture) throw new Error('Destroyed current texture identity changed early');
      let refused=false;try{texture.createView();}catch(e){refused=e instanceof TypeError;}
      if(!refused||canvasPixel(source)!=='0,255,0,255') throw new Error('Destroy lost the drawing buffer or allowed a new view');
      d.pushErrorScope('validation');
      const encoder=d.createCommandEncoder();const pass=encoder.beginRenderPass({colorAttachments:[{
        view:oldView,clearValue:[1,0,0,1],loadOp:'clear',storeOp:'store'
      }]});pass.end();d.queue.submit([encoder.finish()]);
      const error=await d.popErrorScope();
      if(!(error instanceof GPUValidationError)) throw new Error('Writing through a destroyed canvas texture view was accepted');
      const result=canvasPixel(source);gpu.unconfigure();d.destroy();return result;
    "#
        ),
        "0,255,0,255"
    );
}

#[test]
fn webgpu_canvas_withdrawal_pauses_presentation_until_registered_again() {
    let _host = HostGuard::new();
    let c = ctx();
    let key = 9_001_004;
    assert_eq!(
        run_canvas(
            &c,
            r#"
      globalThis.gpu=__rjs_webgpu_context({},1,1,9001004);
      gpu.configure({device:d,format:'rgba8unorm'});paintCanvas(gpu,[1,0,0,1]);return true;
    "#
        ),
        "true"
    );
    let (red, image) = frame(&c, key, 0, (1, 1));
    assert_eq!(image.rgba, vec![255, 0, 0, 255]);
    canvas::unregister_present(key);
    assert!(canvas::present_size(key).is_none());
    assert!(canvas::present(key, 0).is_none());
    let _ = canvas::take_presented_changed();
    ok(&c, "paintCanvas(gpu,[0,0,1,1]);true");
    for _ in 0..3 {
        unsafe { pump(&c) };
    }
    assert!(
        !canvas::take_presented_changed(),
        "Withdrawn canvases should not request repaint"
    );
    canvas::register_present(key);
    assert!(canvas::take_presented_changed());
    let (_, image) = frame(&c, key, red, (1, 1));
    assert_eq!(image.rgba, vec![0, 0, 255, 255]);
    assert_eq!(canvas::present_size(key), Some((1, 1)));
    canvas::unregister_present(key);
    ok(&c, "gpu.unconfigure();d.destroy();true");
}

#[test]
fn webgpu_canvas_transfer_to_bitmap_resets_its_drawing_buffer() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_canvas(
            &c,
            r#"
      const source=new OffscreenCanvas(2,1);const gpu=source.getContext('webgpu');
      gpu.configure({device:d,format:'bgra8unorm',alphaMode:'premultiplied'});
      const old=gpu.getCurrentTexture();paintCanvas(gpu,[1,0,0,1]);
      const bitmap=source.transferToImageBitmap();
      if(bitmap.width!==2||bitmap.height!==1||canvasPixel(bitmap)!=='255,0,0,255') throw new Error('Transfer snapshot was wrong');
      const after=canvasPixel(source);
      const next=gpu.getCurrentTexture();
      let refused=false;try{old.createView();}catch(e){refused=e instanceof TypeError;}
      const result=after==='0,0,0,0'&&next!==old&&refused;
      bitmap.close();gpu.unconfigure();d.destroy();return result;
    "#
        ),
        "true"
    );
}

#[test]
fn webgpu_canvas_expires_after_the_whole_animation_frame_batch() {
    let _host = HostGuard::new();
    let c = ctx();
    let key = 9_001_005;
    assert_eq!(
        run_canvas(
            &c,
            r#"
      globalThis.gpu=__rjs_webgpu_context({},1,1,9001005);gpu.configure({device:d,format:'rgba8unorm'});
      globalThis.sameFrame=false;globalThis.microSame=false;
      requestAnimationFrame(()=>{globalThis.rafTexture=gpu.getCurrentTexture();paintCanvas(gpu,[1,0,0,1]);});
      requestAnimationFrame(()=>{
        sameFrame=gpu.getCurrentTexture()===rafTexture;paintCanvas(gpu,[0,0,1,1]);
        Promise.resolve().then(()=>{microSame=gpu.getCurrentTexture()===rafTexture;});
      });return true;
    "#
        ),
        "true"
    );
    pump_frame(&c);
    ok(&c, "sameFrame&&microSame");
    assert!(texture_destroyed(&c, "rafTexture"));
    let (_, image) = frame(&c, key, 0, (1, 1));
    assert_eq!(image.rgba, vec![0, 0, 255, 255]);
    ok(&c,"(()=>{try{rafTexture.createView();return false;}catch(e){return e instanceof TypeError;}})()");
    canvas::unregister_present(key);
    ok(&c, "gpu.unconfigure();d.destroy();true");
}
