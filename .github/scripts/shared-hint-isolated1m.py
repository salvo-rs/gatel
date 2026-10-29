"""Production-build A/B, same runner, balanced phase order, exact byte checks."""
import hashlib,io,json,os,re,statistics,subprocess,sys,threading,time,zipfile
from pathlib import Path
ROOT=Path.cwd()
HERE=ROOT/"benchmarks/http-servers"
OUT=Path(os.environ["BENCHMARK_OUTPUT"])
BASE="f11ffccdaaae6249c1f779160b5b84af1f479ae6"
CANDIDATE="f0bee4835423e9c0084dff50ceac2fa95e634d9e"
SCENARIOS=["static-1m"]
ORDER=["base","candidate","candidate","base","candidate","base","base","candidate"]
def run(args,**kw):
    print("$"," ".join(map(str,args)),flush=True)
    return subprocess.run(list(map(str,args)),check=True,**kw)
def digest(p): return hashlib.sha256(Path(p).read_bytes()).hexdigest()
def build():
    import hashlib,io,json,os,subprocess,sys,zipfile
    from pathlib import Path
    root=ROOT;out=OUT
    for attempt in range(3):
        try:
            blob=subprocess.check_output(['gh','api','repos/salvo-rs/gatel/actions/artifacts/12927637047/zip'],timeout=90)
            z=zipfile.ZipFile(io.BytesIO(blob));break
        except (subprocess.CalledProcessError,subprocess.TimeoutExpired,zipfile.BadZipFile):
            if attempt==2:raise
    manifest=json.loads(z.read('build-manifest.json'))
    assert manifest['base_revision']=='f11ffccdaaae6249c1f779160b5b84af1f479ae6'
    assert manifest['candidate_revision']=='f0bee4835423e9c0084dff50ceac2fa95e634d9e'
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
    manifest["source_execution_revision"]=manifest["execution_revision"]
    manifest["execution_revision"]=os.environ["GITHUB_SHA"]
    manifest["source_artifact"]=12927637047
    manifest["phase_order"]=ORDER
    manifest["scenarios"]=SCENARIOS
    manifest["purpose"]="Isolated1MiBcontrol ONLY, eightABBA BAAB samples, four/binary60s after10swarmup; eachphasefreshserverandNO precedingRangeorbulkworkload. Exactfinalf0/baseproductionbinaries/defaultclient5s. Investigate mixedsequence higher-quantile signal; retain originalmixedresults anddoNOTpoolVMs."
    (OUT/"build-manifest.json").write_text(json.dumps(manifest,indent=2))
    (OUT/"image-inspect.json").write_text(run(["docker","image","inspect","gatel-ab:base","gatel-ab:candidate"],capture_output=True,text=True).stdout)

def launcher():
    path=OUT/"ab-harness.py"
    path.write_text('''import json,os,sys,threading,time,subprocess,copy
from pathlib import Path
sys.path.insert(0,'''+repr(str(HERE))+''')
import run as bench
bench.git_revision=lambda:os.environ["GATEL_AB_REVISION"]
for name,size in [("16k",16384),("32k",32768),("63k",64512),("64k-plus1",65537),("4m",4194304),("8m-minus1",8388607),("8m-plus1",8388609),("20m",20971520)]:
    filename="payload-"+name+".bin"
    bench.PAYLOADS[filename]=size
    bench.SCENARIOS["static-"+name]={"description":"Boundary control "+name,"service":"{target}-static","path":"/"+filename}
bench.SCENARIOS["range-bulk-offset"]={"description":"8MiB+1 selected at offset1MiB+13","service":"{target}-static","path":"/payload-20m.bin","wrk_script":"/workspace/range-bulk-offset.lua","range":(1048589,9437197)}
for suffix in ["1m","10m"]:
    bench.SCENARIOS["proxy-"+suffix]={"description":"Large reverse proxy control "+suffix,"service":"{target}-proxy","path":"/payload-"+suffix+".bin"}
original=bench.benchmark_target
resources=[]
resource_path=Path(sys.argv[sys.argv.index("--output-dir")+1])/"process-resources.json"
def measured(url,args,script):
    if hasattr(args,"output_dir") and (url.endswith("/payload-10m.bin") or url.endswith("/payload-20m.bin")) and script is None:
        args=copy.copy(args);args.duration=90
    service="gatel-proxy" if "gatel-proxy" in url else "gatel-static"
    container=bench.compose("ps","-q",service,capture_output=True).stdout.strip()
    pid=subprocess.check_output(["docker","inspect","--format","{{.State.Pid}}",container],text=True).strip()
    bench_id=bench.compose("ps","-q","bench",capture_output=True).stdout.strip()
    bench_pid=subprocess.check_output(["docker","inspect","--format","{{.State.Pid}}",bench_id],text=True).strip()
    processes={service:pid,"bench":bench_pid}
    groups={}
    for name,process in processes.items():
        line=next(x for x in Path("/proc/"+process+"/cgroup").read_text().splitlines() if x.startswith("0::"))
        groups[name]=Path("/sys/fs/cgroup")/line.split("::",1)[1].lstrip("/")
    def counters(process):
        result={}
        for filename in ["snmp","netstat"]:
            lines=subprocess.check_output(["sudo","cat","/proc/"+process+"/net/"+filename],text=True).splitlines()
            for i in range(0,len(lines)-1,2):
                names=lines[i].split();values=lines[i+1].split()
                if names[0]!=values[0]:continue
                for key,value in zip(names[1:],values[1:]):
                    try:result[names[0].rstrip(":")+"/"+key]=int(value)
                    except ValueError:pass
        return result
    before={name:counters(process) for name,process in processes.items()}
    def groupstats(path):
        record={}
        for filename in ["memory.stat","cpu.stat"]:
            try:
                for line in (path/filename).read_text().splitlines():
                    key,value=line.split();record[filename+"/"+key]=int(value)
            except OSError:pass
        return record
    stopped=threading.Event()
    samples=[]
    def sample():
        while not stopped.is_set():
            try:
                status=Path("/proc/"+pid+"/status").read_text()
                rss=int(next(line.split()[1] for line in status.splitlines() if line.startswith("VmRSS:")))*1024
                stat=Path("/proc/"+pid+"/stat").read_text().split()
                samples.append({"monotonic":time.monotonic(),"unix_ms":time.time()*1000,"rss_bytes":rss,"threads":int(next(line.split()[1] for line in status.splitlines() if line.startswith("Threads:"))),"cpu_ticks":int(stat[13])+int(stat[14]),"groups":{name:groupstats(path) for name,path in groups.items()}})
            except (OSError,StopIteration): pass
            stopped.wait(1)
    sampler=threading.Thread(target=sample,daemon=True)
    sampler.start()
    try: return original(url,args,script or "/workspace/tail-stats.lua")
    finally:
        stopped.set();sampler.join()
        after={name:counters(process) for name,process in processes.items()}
        resources.append({"tcp_delta":{name:{key:value-before[name].get(key,0) for key,value in values.items()} for name,values in after.items()},"url":url,"duration":args.duration,"kind":"measurement" if hasattr(args,"output_dir") else "warmup","clock_ticks_per_second":os.sysconf("SC_CLK_TCK"),"samples":samples})
        resource_path.write_text(json.dumps(resources,indent=2))
bench.benchmark_target=measured
raise SystemExit(bench.main())
''')
    return path
def environment(name):
    done = """
done = function(summary, latency, requests)
  io.write(string.format("TAIL_DIAG p999_us=%d p9999_us=%d max_us=%d completed=%d timeout=%d\\n",
    latency:percentile(99.9), latency:percentile(99.99), latency.max, summary.requests, summary.errors.timeout))
end
"""
    mounts=[]
    for filename in ["tail-stats.lua","range-64k.lua","range-offset-64k.lua","range-65k.lua","range-bulk-offset.lua"]:
        prefix = 'wrk.headers["Accept-Encoding"] = "identity"\n' if filename == "tail-stats.lua" else 'wrk.headers["Accept-Encoding"] = "identity"\nwrk.headers["Range"] = "bytes=1048589-9437197"\n' if filename == "range-bulk-offset.lua" else (HERE/"fixtures"/filename).read_text()
        path=OUT/filename
        path.write_text(prefix+done)
        mounts.append("      - "+str(path)+":/workspace/"+filename+":ro")
    override=OUT/(name+"-compose.yml")
    override.write_text("services:\n  gatel-static:\n    image: gatel-ab:"+name+"\n  gatel-proxy:\n    image: gatel-ab:"+name+"\n  bench:\n    volumes:\n"+"\n".join(mounts)+"\n")
    return dict(os.environ,GATEL_AB_REVISION=(BASE if name=="base" else CANDIDATE),COMPOSE_FILE=str(HERE/"compose.yml")+":"+str(override))

def measure():
    launch=launcher();rows=[]
    for phase,name in enumerate(ORDER,1):
        directory=OUT/f"screen-{phase:02d}-{name}";directory.mkdir()
        command=[sys.executable,"-B","-u",launch,"--target","gatel","--no-build","--rounds","1","--duration","60","--warmup","10","--threads","4","--connections","128","--output-dir",directory]
        for scenario in SCENARIOS:command.extend(["--scenario",scenario])
        with (directory/"run.log").open("w") as log:
            process=subprocess.Popen(list(map(str,command)),env=environment(name),stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            for line in process.stdout:print(line,end="",flush=True);log.write(line);log.flush()
            if process.wait():raise RuntimeError("Replication phase failed; all data retained")
        part=json.loads((directory/"summary.json").read_text())
        assert len(part)==len(json.loads((directory/"payload-checks.json").read_text()))==1
        for row in part:
            raw=(directory/"raw"/f'{row["scenario"]}-gatel-round1.txt').read_text()
            extra=re.search(r"TAIL_DIAG p999_us=(\d+) p9999_us=(\d+) max_us=(\d+) completed=(\d+) timeout=(\d+)",raw)
            assert extra,raw
            row.update(p999_ms=int(extra[1])/1000,p9999_ms=int(extra[2])/1000,histogram_max_ms=int(extra[3])/1000,variant=name,phase=phase,suite="screen")
            assert int(extra[4])==row["total_requests"] and int(extra[5])==row["timeout_errors"]
        rows.extend(part);(OUT/"comparison.json").write_text(json.dumps(rows,indent=2))
        report=directory/"report.md"
        report.write_text(report.read_text().replace("1 round(s) of 20s each","1 round per case:10m/20m=90s, other=20s"))
    assert len(rows)==8
    assert all(r["socket_errors"]==r["non_2xx_3xx"]==0 for r in rows if r["variant"]=="candidate"),"Candidate error; reject, all windows retained"

OUT.mkdir(parents=True,exist_ok=True)
{"build":build,"measure":measure}[sys.argv[1]]()

