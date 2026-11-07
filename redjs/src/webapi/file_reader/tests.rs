use crate::webapi::{
    file::testing::{file_ctx, run, TempFile},
    testing::*,
};

const READ: &str = r#"async function read(blob, method='readAsText', label) {
  const r=new FileReader();
  return await new Promise((resolve,reject)=>{r.onload=()=>resolve(r.result);r.onerror=()=>reject(r.error);r[method](blob,label)});
}"#;
#[test]
fn native_interface_constants_brand_and_subclasses() {
    let c = file_ctx();
    ok(&c,"(() => {class Sub extends FileReader{};const r=new Sub();return r instanceof Sub&&r instanceof FileReader&&r instanceof EventTarget&&Object.prototype.toString.call(r)==='[object FileReader]'&&[FileReader.EMPTY,FileReader.LOADING,FileReader.DONE,r.EMPTY,r.LOADING,r.DONE,r.readyState,r.result,r.error].join()==='0,1,2,0,1,2,0,,'})()");
    ok(&c,"String(FileReader).includes('[native code]')&&String(FileReader.prototype.readAsText).includes('[native code]')");
    assert!(throws(&c, "FileReader()").contains("new"));
    assert!(
        throws(&c, "FileReader.prototype.readAsText.call({},new Blob())")
            .contains("non-FileReader")
    );
    assert!(throws(
        &c,
        "Object.getOwnPropertyDescriptor(FileReader.prototype,'readyState').get.call({})"
    )
    .contains("non-FileReader"));
    ok(&c,"(() => {const r=new FileReader();try{r.result=1}catch{};return r.result===null&&Object.getOwnPropertyDescriptor(FileReader,'DONE').writable===false&&Object.getOwnPropertyDescriptor(FileReader,'DONE').enumerable===true&&Object.getOwnPropertyDescriptor(FileReader.prototype,'DONE').enumerable===true})()");
}
#[test]
fn read_events_are_async_trusted_progress_events_with_visible_state() {
    let c = file_ctx();
    assert_eq!(
        run(
            &c,
            r#"const r=new FileReader(),seen=[];for(const t of ['loadstart','progress','load','loadend'])r.addEventListener(t,e=>{if(!(e instanceof ProgressEvent)||!e.isTrusted||e.target!==r)throw Error('wrong native event');seen.push([t,r.readyState,r.result===null?'null':'set',e.loaded,e.total].join(':'))});const end=new Promise(resolve=>r.onloadend=resolve);r.readAsText(new Blob(['héllo']));if(r.readyState!==1||seen.length)throw Error('synchronous events');await end;return seen.join('|')"#
        ),
        "loadstart:1:null:0:0|progress:1:null:6:6|load:2:set:6:6|loadend:2:set:6:6"
    );
}
#[test]
fn all_read_modes_and_empty_blobs_are_packaged_in_rust() {
    let c = file_ctx();
    let body=format!("{READ} const b=new Blob([new Uint8Array([0,255])]);const ab=await read(b,'readAsArrayBuffer');return [ab instanceof ArrayBuffer,new Uint8Array(ab).join(),(await read(b,'readAsBinaryString')).charCodeAt(1),await read(new Blob(['hi'],{{type:'image/png'}}),'readAsDataURL'),await read(new Blob(['hi']),'readAsDataURL'),(await read(new Blob(),'readAsArrayBuffer')).byteLength,await read(new Blob())].join('|')");
    assert_eq!(
        run(&c, &body),
        "true|0,255|255|data:image/png;base64,aGk=|data:application/octet-stream;base64,aGk=|0|"
    );
}
#[test]
fn text_labels_charset_and_bom_have_the_expected_precedence() {
    let c = file_ctx();
    let body=format!("{READ} const b=new Blob([new Uint8Array([207,240,232,226,229,242])],{{type:'text/plain; charset=windows-1251'}});return [await read(b,'readAsText','windows-1251'),await read(b),await read(b,'readAsText','not-an-encoding'),await read(new Blob([new Uint8Array([255,254,65,0])]),'readAsText','windows-1251'),await read(new Blob([new Uint8Array([254,255,0,66])])),await read(new Blob([new Uint8Array([239,187,191,67])])),await read(new Blob([new Uint8Array([255])]))].join('|')");
    assert_eq!(run(&c, &body), "Привет|Привет|Привет|A|B|C|�");
}
#[test]
fn idl_validation_and_loading_state_reject_invalid_calls() {
    let c = file_ctx();
    ok(&c,"(() => {const r=new FileReader();let missing=false,type=false,busy=false,symbol=false;try{r.readAsText()}catch(e){missing=e instanceof TypeError};try{r.readAsText({arrayBuffer(){}})}catch(e){type=e instanceof TypeError};try{r.readAsText(new Blob(),Symbol())}catch(e){symbol=e instanceof TypeError};r.readAsText(new Blob(['x']));try{r.readAsText(new Blob(['y']))}catch(e){busy=e instanceof DOMException&&e.name==='InvalidStateError'};r.abort();return missing&&type&&busy&&symbol})()");
    assert_eq!(
        run(
            &c,
            r#"const r=new FileReader();let busy=false;try{r.readAsText(new Blob(['outer']),{toString(){r.readAsText(new Blob(['inner']));return 'utf-8'}})}catch(e){busy=e.name==='InvalidStateError'};await new Promise(resolve=>r.onloadend=resolve);return busy+':'+r.result"#
        ),
        "true:inner"
    );
}
#[test]
fn abort_cancels_queued_work_and_reentrant_abort_suppresses_old_loadend() {
    let c = file_ctx();
    assert_eq!(
        run(
            &c,
            r#"const r=new FileReader(),seen=[];for(const t of ['loadstart','load','abort','loadend'])r.addEventListener(t,()=>seen.push(t));r.readAsText(new Blob(['x']));r.abort();await new Promise(resolve=>setTimeout(resolve,20));return [r.readyState,r.result,seen.join()].join('|')"#
        ),
        "2||abort,loadend"
    );
    assert_eq!(
        run(
            &c,
            r#"const r=new FileReader(),seen=[];let complete;const done=new Promise(resolve=>complete=resolve);r.onabort=()=>{seen.push('abort');r.readAsText(new Blob(['new']))};r.onload=()=>seen.push('load:'+r.result);r.onloadend=()=>{seen.push('end:'+r.result);if(r.result==='new')complete()};r.readAsText(new Blob(['old']));r.abort();await done;return seen.join('|')"#
        ),
        "abort|load:new|end:new"
    );
}
#[test]
fn abort_during_progress_prevents_load_and_reentrant_load_suppresses_old_end() {
    let c = file_ctx();
    assert_eq!(
        run(
            &c,
            r#"const r=new FileReader(),seen=[];let complete;const done=new Promise(resolve=>complete=resolve);r.onprogress=()=>r.abort();r.onabort=()=>seen.push('abort');r.onload=()=>seen.push('load');r.onloadend=()=>{seen.push('end');complete()};r.readAsText(new Blob(['x']));await done;return seen.join()"#
        ),
        "abort,end"
    );
    assert_eq!(
        run(
            &c,
            r#"const r=new FileReader(),seen=[];let complete;const done=new Promise(resolve=>complete=resolve);r.onload=()=>{seen.push('load:'+r.result);if(r.result==='one')r.readAsText(new Blob(['two']))};r.onloadend=()=>{seen.push('end:'+r.result);if(r.result==='two')complete()};r.readAsText(new Blob(['one']));await done;return seen.join('|')"#
        ),
        "load:one|load:two|end:two"
    );
}
#[test]
fn native_blob_read_ignores_overridden_js_arraybuffer_and_properties() {
    let c = file_ctx();
    assert_eq!(run(&c,&format!("{READ} class Custom extends Blob{{arrayBuffer(){{throw Error('must not call JS')}}get type(){{throw Error('must not read JS')}}}}return await read(new Custom(['native bytes']))")),"native bytes");
}
#[test]
fn missing_or_changed_disk_files_deliver_the_native_domexception() {
    let c = file_ctx();
    let t = TempFile::new("reader.txt", b"original");
    t.expose(&c, "picked");
    std::fs::write(&t.0, b"changed size").unwrap();
    assert_eq!(
        run(
            &c,
            r#"const r=new FileReader(),seen=[];for(const t of ['loadstart','load','error','loadend'])r.addEventListener(t,()=>seen.push(t));const done=new Promise(resolve=>r.onloadend=resolve);r.readAsText(picked);await done;return [r.readyState,r.result,r.error instanceof DOMException,r.error.name,seen.join()].join('|')"#
        ),
        "2||true|NotReadableError|loadstart,error,loadend"
    );
    std::fs::remove_file(&t.0).unwrap();
    assert_eq!(
        run(
            &c,
            &format!("{READ} try{{await read(picked)}}catch(e){{return e.name}}")
        ),
        "NotFoundError"
    );
}
#[test]
fn handler_replacement_once_listeners_and_abort_clear_done_result() {
    let c = file_ctx();
    assert_eq!(
        run(
            &c,
            r#"const r=new FileReader();let n=0;r.onload=()=>n++;r.onload=()=>n+=10;r.addEventListener('load',()=>n+=100,{once:true});await new Promise(resolve=>{r.onloadend=resolve;r.readAsText(new Blob(['x']))});await new Promise(resolve=>{r.onloadend=resolve;r.readAsText(new Blob(['y']))});r.abort();r.onprogress=5;return [n,r.result,r.error,r.readyState,r.onprogress].join('|')"#
        ),
        "120|||2|"
    );
}
#[test]
fn active_reader_and_its_callbacks_survive_gc_then_release_the_flight() {
    let c = file_ctx();
    s(&c,"globalThis.__readerDone='pending';(()=>{const capture={text:'retained'};let r=new FileReader();r.onload=()=>{__readerDone=r.result+':'+capture.text};r.readAsText(new Blob(['native']));})()");
    c.gc();
    assert_eq!(run(&c,"while(__readerDone==='pending')await new Promise(resolve=>setTimeout(resolve,1));return __readerDone"),"native:retained");
    assert_eq!(
        super::flights()
            .lock()
            .unwrap()
            .keys()
            .filter(|(key, _)| *key == c.raw() as usize)
            .count(),
        0
    );
    s(&c,"globalThis.__reader=new FileReader();__reader.onloadend=()=>{};__reader.readAsArrayBuffer(new Blob(['kept']))");
    assert_eq!(run(&c,"while(__reader.readyState!==2)await new Promise(resolve=>setTimeout(resolve,1));return __reader.result.byteLength"),"4");
    c.gc();
    ok(
        &c,
        "new Uint8Array(__reader.result).join()==='107,101,112,116'",
    );
}
#[test]
fn readers_and_subclasses_are_not_structured_cloneable_without_desktop_host() {
    let c = file_ctx();
    ok(&c,"(() => {class Sub extends FileReader{};for(const value of [new FileReader(),new Sub(),{nested:new Sub()}]){try{structuredClone(value);return false}catch(e){if(!(e instanceof DOMException)||e.name!=='DataCloneError')return false}}return true})()");
}

#[test]
fn arraybuffer_and_text_packaging_ignore_replaced_javascript_intrinsics() {
    let c = file_ctx();
    assert_eq!(run(&c,&format!("{READ} Object.defineProperty(Object.getPrototypeOf(Uint8Array.prototype),'buffer',{{get(){{throw Error('replaced getter')}}}});globalThis.TextDecoder=function(){{throw Error('replaced decoder')}};const b=await read(new Blob(['abc']),'readAsArrayBuffer');return b.byteLength+':'+await read(new Blob(['native text']))")),"3:native text");
}
