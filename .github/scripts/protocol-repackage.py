import hashlib,io,json,os,subprocess,sys,zipfile
from pathlib import Path
root=Path.cwd();out=Path(os.environ['BENCHMARK_OUTPUT'])
for attempt in range(3):
    try:
        blob=subprocess.check_output(['gh','api','repos/salvo-rs/gatel/actions/artifacts/12894765238/zip'],timeout=90)
        z=zipfile.ZipFile(io.BytesIO(blob));break
    except (subprocess.CalledProcessError,subprocess.TimeoutExpired,zipfile.BadZipFile):
        if attempt==2:raise
manifest=json.loads(z.read('build-manifest.json'))
assert manifest['base_revision']=='f11ffccdaaae6249c1f779160b5b84af1f479ae6'
assert manifest['candidate_revision']=='b8d2a1f28eaefc80b48150287643a8fdd979f0df'
assert manifest['images']['base']['compiler']==manifest['images']['candidate']['compiler']
for filename,key in [('Dockerfile','dockerfile_sha256'),('Cargo.lock','lock_sha256')]:
    assert hashlib.sha256((root/filename).read_bytes()).hexdigest()==manifest[key]
for name in ['base','candidate']:
    binary=z.read(name+'-gatel');assert hashlib.sha256(binary).hexdigest()==manifest['images'][name]['binary_sha256']
    (out/(name+'-gatel')).write_bytes(binary)
    ctx=out/(name+'-context');p=ctx/'context/linux/amd64/gatel';p.parent.mkdir(parents=True);p.write_bytes(binary)
    (ctx/'Dockerfile').write_bytes((root/'Dockerfile').read_bytes())
    subprocess.run(['docker','build','--build-arg','SOURCE=prebuilt','--build-arg','TARGETPLATFORM=linux/amd64','-t','gatel-ab:'+name,str(ctx)],check=True)
(out/'build-manifest.json').write_text(json.dumps(manifest,indent=2))
subprocess.run(['docker','compose','-f',str(root/'benchmarks/http-servers/compose.yml'),'build','bench'],check=True)
sys.path.insert(0,str(root/'benchmarks/http-servers'));import run
run.ensure_payloads()
