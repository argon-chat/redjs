//! External image uploads qualified through real JSC and GPU readback.
use super::*;

const HELPERS: &str = r#"
  globalThis.makeTexture=(format='rgba8unorm',size=[2,2],extra={})=>d.createTexture({format,size,
    usage:GPUTextureUsage.COPY_DST|GPUTextureUsage.RENDER_ATTACHMENT|GPUTextureUsage.COPY_SRC|GPUTextureUsage.TEXTURE_BINDING,...extra});
  globalThis.readTexture=async(texture,width,height,bytesPerPixel=4,mipLevel=0,origin={})=>{
    const stride=Math.ceil(width*bytesPerPixel/256)*256;
    const buffer=d.createBuffer({size:stride*height,usage:GPUBufferUsage.COPY_DST|GPUBufferUsage.MAP_READ});
    const encoder=d.createCommandEncoder();encoder.copyTextureToBuffer({texture,mipLevel,origin},{buffer,bytesPerRow:stride},[width,height]);d.queue.submit([encoder.finish()]);
    await buffer.mapAsync(GPUMapMode.READ);const raw=new Uint8Array(buffer.getMappedRange());const out=[];
    for(let y=0;y<height;y++) out.push(...raw.slice(y*stride,y*stride+width*bytesPerPixel));
    buffer.unmap();buffer.destroy();return new Uint8Array(out);
  };
"#;
fn external(c: &Context, body: &str) -> String {
    run_gpu(c, &format!("{HELPERS}\n{body}"))
}

#[test]
fn webgpu_external_image_data_crop_flip_and_destination_subresource() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        external(
            &c,
            r#"
      const image=new ImageData(new Uint8ClampedArray([
        1,2,3,255, 4,5,6,255, 7,8,9,255,
        11,12,13,255, 14,15,16,255, 17,18,19,255,
        21,22,23,255, 24,25,26,255, 27,28,29,255]),3,3);
      const t=makeTexture('rgba8unorm',[8,8,2],{mipLevelCount:3});
      d.queue.copyExternalImageToTexture({source:image,origin:[1,1],flipY:true},{texture:t,mipLevel:1,origin:[1,1,1]},[2,2]);
      const pixels=await readTexture(t,2,2,4,1,[1,1,1]);t.destroy();d.destroy();return Array.from(pixels).join(',');
    "#
        ),
        "24,25,26,255,27,28,29,255,14,15,16,255,17,18,19,255"
    );
}
#[test]
fn webgpu_external_bitmap_preserves_straight_pixels_and_owns_snapshot() {
    let _host = HostGuard::new();
    let c = ctx();
    install_gc(&c);
    assert_eq!(
        external(
            &c,
            r#"
      const data=new ImageData(new Uint8ClampedArray([199,17,3,64, 9,20,30,0]),2,1);
      const image=await createImageBitmap(data);const t=makeTexture('rgba8unorm',[2,1]);
      d.queue.copyExternalImageToTexture({source:image},{texture:t},[2,1]);
      image.close();data.data.fill(0);__forceGpuGc();
      const pixels=await readTexture(t,2,1);t.destroy();d.destroy();return Array.from(pixels).join(',');
    "#
        ),
        "199,17,3,64,9,20,30,0"
    );
}
#[test]
fn webgpu_external_formats_alpha_srgb_and_float_encoding() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        external(
            &c,
            r#"
      const image=new ImageData(new Uint8ClampedArray([128,64,32,128]),1,1);const results=[];
      for(const format of ['rgba8unorm','rgba8unorm-srgb','bgra8unorm','bgra8unorm-srgb','r8unorm','rg8unorm']){
        const t=makeTexture(format,[1,1]);d.queue.copyExternalImageToTexture({source:image},{texture:t,premultipliedAlpha:true},[1,1]);
        const n=format==='r8unorm'?1:format==='rg8unorm'?2:4;results.push(Array.from(await readTexture(t,1,1,n)).join(','));t.destroy();
      }
      for(const [format,n]of [['r16float',1],['rg16float',2],['rgba16float',4]]){
        const t=makeTexture(format,[1,1]);const white=new ImageData(new Uint8ClampedArray([255,255,255,255]),1,1);
        d.queue.copyExternalImageToTexture({source:white},{texture:t},[1,1]);const words=new Uint16Array((await readTexture(t,1,1,n*2)).buffer);
        if(!Array.from(words).every(x=>x===0x3c00))throw Error('Incorrect f16 '+format);t.destroy();
      }
      const packed=makeTexture('rgb10a2unorm',[1,1]);d.queue.copyExternalImageToTexture({source:image},{texture:packed},[1,1]);
      const bits=new Uint32Array((await readTexture(packed,1,1)).buffer)[0];if(bits!==(514|(257<<10)|(128<<20)|(2<<30))>>>0)throw Error('Incorrect rgb10a2 '+bits);packed.destroy();
      for(const [format,n]of [['r32float',1],['rg32float',2],['rgba32float',4]]){
        const t=makeTexture(format,[1,1]);d.queue.copyExternalImageToTexture({source:image},{texture:t},[1,1]);
        const floats=new Float32Array((await readTexture(t,1,1,n*4)).buffer);const want=[128/255,64/255,32/255,128/255];
        if(!Array.from(floats).every((x,i)=>Math.abs(x-want[i])<1e-6))throw Error('Incorrect float '+format);t.destroy();
      }
      d.destroy();return results.join(';');
    "#
        ),
        "64,32,16,128;64,32,16,128;16,32,64,128;16,32,64,128;64;64,32"
    );
}
#[test]
fn webgpu_external_snapshot_runs_after_descriptor_getters_and_rechecks_detachment() {
    let _host = HostGuard::new();
    let c = ctx();
    install_gc(&c);
    assert_eq!(
        external(
            &c,
            r#"
      const image=new ImageData(new Uint8ClampedArray([1,2,3,255]),1,1);const t=makeTexture('rgba8unorm',[1,1]);
      d.queue.copyExternalImageToTexture({source:image},{texture:t,get premultipliedAlpha(){image.data.set([40,50,60,255]);__forceGpuGc();return false;}},[1,1]);
      const result=Array.from(await readTexture(t,1,1)).join(',');
      const detached=new ImageData(1,1);let failed=false;
      try{d.queue.copyExternalImageToTexture({source:detached},{texture:t},[ {valueOf(){structuredClone(detached.data.buffer,{transfer:[detached.data.buffer]});__forceGpuGc();return 1;}},1]);}catch(e){failed=e.name==='InvalidStateError';}
      const bitmap=await createImageBitmap(image);let closed=false;
      try{d.queue.copyExternalImageToTexture({source:bitmap},{texture:t,get premultipliedAlpha(){bitmap.close();__forceGpuGc();return false;}},[1,1]);}catch(e){closed=e.name==='InvalidStateError';}
      let ephemeral=await createImageBitmap(image);
      const ephemeralInfo={get source(){const selected=ephemeral;ephemeral=null;return selected;}};
      d.queue.copyExternalImageToTexture(ephemeralInfo,{texture:t,get premultipliedAlpha(){__forceGpuGc();return false;}},[1,1]);
      if(Array.from(await readTexture(t,1,1)).join(',')!=='40,50,60,255')throw Error('Unrooted source returned by getter');
      t.destroy();d.destroy();return result+';'+failed+';'+closed;
    "#
        ),
        "40,50,60,255;true;true"
    );
}
#[test]
fn webgpu_external_invalid_descriptors_and_destination_errors_are_distinct() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        external(
            &c,
            r#"
      const image=new ImageData(1,1);const t=makeTexture('rgba8unorm',[1,1]);const errors=[];
      for(const [src,dst,size]of [[{source:image,origin:[0,0,0]},{texture:t},[1,1]],[{source:image},{texture:t},[2,1]],[{source:image},{texture:t},[1,1,2]],[{source:{}},{texture:t},[1,1]],[{source:image},{texture:t,colorSpace:'display-p3'},[1,1]]]){
        try{d.queue.copyExternalImageToTexture(src,dst,size);errors.push('none');}catch(e){errors.push(e.name);}
      }
      for(const bad of [makeTexture('rgba8unorm',[1,1],{usage:GPUTextureUsage.COPY_DST}),makeTexture('rgba8uint',[1,1]),makeTexture('rgba8unorm',[1,1])]){
        if(bad.usage&GPUTextureUsage.RENDER_ATTACHMENT && bad.format==='rgba8unorm') bad.destroy();
        d.pushErrorScope('validation');d.queue.copyExternalImageToTexture({source:image},{texture:bad},[1,1]);const e=await d.popErrorScope();if(!(e instanceof GPUValidationError))throw Error('Missing target validation');bad.destroy();
      }
      d.pushErrorScope('validation');d.queue.copyExternalImageToTexture({source:image},{texture:t,origin:[1,0]},[1,1]);if(!(await d.popErrorScope() instanceof GPUValidationError))throw Error('Missing range validation');
      d.pushErrorScope('validation');d.queue.copyExternalImageToTexture({source:image},{texture:t},[0,1]);if(await d.popErrorScope()!==null)throw Error('Zero copy must be accepted');
      t.destroy();d.destroy();return errors.join(',');
    "#
        ),
        "TypeError,OperationError,OperationError,TypeError,NotSupportedError"
    );
}

#[cfg(feature = "canvas")]
#[test]
fn webgpu_external_2d_and_gpu_canvases_preserve_premultiplied_channels() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        external(
            &c,
            r#"
      const two=new OffscreenCanvas(1,1);const cx=two.getContext('2d');cx.fillStyle='rgba(128,64,32,0.5)';cx.fillRect(0,0,1,1);
      const a=makeTexture('rgba8unorm',[1,1]);d.queue.copyExternalImageToTexture({source:two},{texture:a,premultipliedAlpha:true},[1,1]);
      const pixel=Array.from(await readTexture(a,1,1));if(!pixel.every((x,i)=>Math.abs(x-[64,32,16,128][i])<=1))throw Error('2D alpha conversion '+pixel);
      const source=new OffscreenCanvas(1,1);const gpu=source.getContext('webgpu');gpu.configure({device:d,format:'rgba8unorm',alphaMode:'premultiplied'});
      const encoder=d.createCommandEncoder();const pass=encoder.beginRenderPass({colorAttachments:[{view:gpu.getCurrentTexture().createView(),loadOp:'clear',storeOp:'store',clearValue:[200/255,50/255,5/255,64/255]}]});pass.end();d.queue.submit([encoder.finish()]);
      const b=makeTexture('rgba8unorm',[1,1]);d.queue.copyExternalImageToTexture({source},{texture:b,premultipliedAlpha:true},[1,1]);const exact=Array.from(await readTexture(b,1,1)).join(',');
      const f=makeTexture('rgba32float',[1,1]);d.queue.copyExternalImageToTexture({source},{texture:f},[1,1]);const floats=new Float32Array((await readTexture(f,1,1,16)).buffer);
      if(Math.abs(floats[0]-200/64)>1e-5)throw Error('Premultiplied channel was clamped before float encoding');
      gpu.unconfigure();a.destroy();b.destroy();f.destroy();d.destroy();return exact;
    "#
        ),
        "200,50,5,64"
    );
}
#[cfg(feature = "canvas")]
#[test]
fn webgpu_external_native_html_image_and_blank_mirror_canvas() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        external(
            &c,
            r#"
      const source=new OffscreenCanvas(1,1);const ctx=source.getContext('2d');ctx.fillStyle='#00ff00';ctx.fillRect(0,0,1,1);
      const url=URL.createObjectURL(await source.convertToBlob());const image=new Image(99,88);image.src=url;await image.decode();
      const t=makeTexture('rgba8unorm',[1,1]);d.queue.copyExternalImageToTexture({source:image},{texture:t},[1,1]);const green=Array.from(await readTexture(t,1,1)).join(',');
      const blank={tagName:'CANVAS',__vnId:9721,width:1,height:1};__rjs_canvas_source_hook(value=>value===blank?null:undefined);
      d.queue.copyExternalImageToTexture({source:blank},{texture:t},[1,1]);const clear=Array.from(await readTexture(t,1,1)).join(',');
      URL.revokeObjectURL(url);t.destroy();d.destroy();return green+';'+clear;
    "#
        ),
        "0,255,0,255;0,0,0,0"
    );
}
#[cfg(feature = "canvas")]
#[test]
fn webgpu_external_origin_clean_metadata_survives_hooks_and_canvas_draws() {
    let _host = HostGuard::new();
    let c = ctx();
    unsafe {
        let bitmap = crate::webapi::image::create_bitmap(
            c.raw(),
            crate::webapi::image::BitmapData {
                width: 1,
                height: 1,
                rgba: vec![255, 0, 0, 255],
                origin_clean: false,
            },
        );
        crate::webapi::util::define(
            c.raw(),
            crate::glue::rjs_global(c.raw()),
            "tainted",
            bitmap,
            0,
        );
    }
    assert_eq!(
        external(
            &c,
            r#"
      const mirror={};__rjs_canvas_source_hook(value=>value===mirror?tainted:undefined);
      const canvas=new OffscreenCanvas(1,1);canvas.getContext('2d').drawImage(tainted,0,0);
      const t=makeTexture('rgba8unorm',[1,1]);const errors=[];
      for(const source of [tainted,mirror,canvas]){try{d.queue.copyExternalImageToTexture({source},{texture:t},[1,1]);errors.push('none');}catch(e){errors.push(e.name);}}
      t.destroy();d.destroy();return errors.join(',');
    "#
        ),
        "SecurityError,SecurityError,SecurityError"
    );
}
#[cfg(feature = "canvas")]
#[test]
fn webgpu_external_source_hooks_are_gc_safe_and_revalidate_destination() {
    let _host = HostGuard::new();
    let c = ctx();
    install_gc(&c);
    assert_eq!(
        external(
            &c,
            r#"
      const mirror={};let texture=makeTexture('rgba8unorm',[1,1]);let mode=0;
      const bitmap=await createImageBitmap(new ImageData(new Uint8ClampedArray([9,8,7,255]),1,1));
      __rjs_canvas_source_hook(value=>{if(value!==mirror)return undefined;__forceGpuGc();if(mode===1)texture.destroy();return bitmap;});
      d.queue.copyExternalImageToTexture({source:mirror},{texture},[1,1]);const result=Array.from(await readTexture(texture,1,1)).join(',');mode=1;
      d.pushErrorScope('validation');d.queue.copyExternalImageToTexture({source:mirror},{texture},[1,1]);if(!(await d.popErrorScope() instanceof GPUValidationError))throw Error('Destination changed in source hook was not rechecked');
      bitmap.close();texture.destroy();d.destroy();return result;
    "#
        ),
        "9,8,7,255"
    );
}
