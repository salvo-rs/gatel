"""Supplemental same-host H2/TLS controls, kept separate from wrk data."""
import hashlib,json,os,re,subprocess,time
from pathlib import Path
ROOT=Path.cwd();HERE=ROOT/'benchmarks/http-servers';OUT=Path(os.environ['BENCHMARK_OUTPUT'])/'protocol-controls'

def run(args,**kw):
    return subprocess.run(list(map(str,args)),check=True,**kw)

def capture(args,**kw):
    return run(args,capture_output=True,**kw).stdout

def main():
    OUT.mkdir()
    context=OUT/'client-context';context.mkdir()
    (context/'Dockerfile').write_text('FROM gatel-http-bench-runner:local\nRUN apk add --no-cache nghttp2-client\n')
    run(['docker','build','-t','gatel-protocol-client:local',context])
    run(['docker','network','create','gatel-protocol-ab'])
    tls=OUT/'tls';tls.mkdir()
    run(['openssl','ecparam','-name','prime256v1','-genkey','-noout','-out',tls/'key.pem'])
    run(['openssl','req','-new','-x509','-key',tls/'key.pem','-out',tls/'cert.pem','-days','2','-subj','/CN=gatel-protocol','-addext','subjectAltName=DNS:gatel-protocol'])
    rows=[];proofs=[]
    for mode in ['h2c','h2-tls','h1-tls']:
        for phase,variant in enumerate(['base','candidate','candidate','base'],1):
            directory=OUT/f'{mode}-{phase:02d}-{variant}';directory.mkdir()
            config=directory/'gatel.kdl'
            secure=mode!='h2c'
            config.write_text('global { '+('https ":8443"' if secure else 'http ":8080"')+'; log level="error" format="json"; }\nsite "gatel-protocol" { '+('tls cert="/certs/cert.pem" key="/certs/key.pem"; ' if secure else '')+'route "/*" { file-server root="/srv/www"; } }\n')
            try:
                run(['docker','run','-d','--network','gatel-protocol-ab','--name','gatel-protocol','--no-healthcheck','-v',str(config)+':/etc/gatel/gatel.kdl:ro','-v',str(tls)+':/certs:ro','-v',str(HERE/'fixtures/www')+':/srv/www:ro','gatel-ab:'+variant])
                run(['docker','run','-d','--network','gatel-protocol-ab','--name','gatel-protocol-client','gatel-protocol-client:local'])
                version=capture(['docker','exec','gatel-protocol-client','h2load','--version'],text=True)
                (directory/'client-version.txt').write_text(version)
                for size in ['64k','1m','10m']:
                    url=('https://gatel-protocol:8443' if secure else 'http://gatel-protocol:8080')+'/payload-'+size+'.bin'
                    body=capture(['docker','exec','gatel-protocol-client','curl','-kfSs','--retry','10','--retry-connrefused','--retry-delay','1',url])
                    source=(HERE/'fixtures/www'/('payload-'+size+'.bin')).read_bytes()
                    assert body==source
                    proofs.append({'mode':mode,'phase':phase,'variant':variant,'size':size,'sha256':hashlib.sha256(body).hexdigest(),'bytes':len(body)})
                    clients,streams=(128,1) if mode=='h1-tls' else (8,16)
                    duration=30 if size=='10m' else 10
                    command=['docker','exec','gatel-protocol-client','h2load','-t4','-c'+str(clients),'-m'+str(streams),'-D'+str(duration),'--warm-up-time=3','-N5','-H','accept-encoding: identity']
                    if mode=='h1-tls':command.append('--h1')
                    raw=capture(command+[url],text=True,timeout=duration+35)
                    (directory/(size+'.txt')).write_text(raw)
                    metric=re.search(r'requests:.*?(\d+) done, (\d+) succeeded, (\d+) failed, (\d+) errored, (\d+) timeout',raw)
                    assert metric,raw
                    done,succeeded,failed,errored,timeout=map(int,metric.groups())
                    rate=re.search(r'finished in .*?, ([\d.]+) req/s',raw)
                    assert rate,raw
                    assert 'Application protocol: '+('h2c' if mode=='h2c' else 'h2' if mode=='h2-tls' else 'http/1.1') in raw,raw
                    rows.append({'mode':mode,'phase':phase,'variant':variant,'size':size,'duration_seconds':duration,'clients':clients,'streams_per_client':streams,'done':done,'succeeded':succeeded,'failed':failed,'errored':errored,'timeout':timeout,'requests_per_sec':float(rate[1])})
                    (OUT/'comparison.json').write_text(json.dumps(rows,indent=2));(OUT/'payload-checks.json').write_text(json.dumps(proofs,indent=2))
                    print(json.dumps(rows[-1]),flush=True)
            finally:
                logs=subprocess.run(['docker','logs','gatel-protocol'],capture_output=True,text=True)
                (directory/'server.log').write_text(logs.stdout+logs.stderr)
                subprocess.run(['docker','rm','-f','gatel-protocol','gatel-protocol-client'],check=False)
    assert len(rows)==len(proofs)==36
    assert all(r['failed']==r['errored']==r['timeout']==0 for r in rows if r['variant']=='candidate'), 'Candidate protocol errors; all data retained.'
    run(['docker','network','rm','gatel-protocol-ab'])

main()
