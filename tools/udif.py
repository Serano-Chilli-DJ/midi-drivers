import sys, struct, zlib, bz2, plistlib, base64, os
p=sys.argv[1]; out=sys.argv[2]
f=open(p,'rb'); f.seek(-512,2); koly=f.read(512)
assert koly[:4]==b'koly', 'no koly trailer'
(sig,ver,hdrsize,flags,runfwd,dataoff,datalen,rsrcoff,rsrclen,segnum,segcnt)=struct.unpack('>4sIIIQQQQQII',koly[:64])
xmloff,xmllen=struct.unpack('>QQ',koly[216:232])
f.seek(xmloff); pl=plistlib.loads(f.read(xmllen))
blk=pl['resource-fork']['blkx']
o=open(out,'wb')
for b in blk:
    d=b['Data']; name=b.get('Name'); print('blkx',name,b.get('ID'))
    assert d[:4]==b'mish'
    sectnum,sectcnt,dataoff2=struct.unpack('>QQQ',d[8:32])
    nchunks=struct.unpack('>I',d[200:204])[0]
    for i in range(nchunks):
        c=d[204+i*40:204+(i+1)*40]
        typ,comment,sec,scount,coff,clen=struct.unpack('>IIQQQQ',c)
        if typ in (0,2): # zero / ignore
            o.seek((sectnum+sec)*512); o.write(b'\0'*(scount*512)); continue
        if typ==0xffffffff: continue
        f.seek(dataoff+coff); raw=f.read(clen)
        if typ==1: data=raw
        elif typ==0x80000004: raise SystemExit('adc unsupported')
        elif typ==0x80000005: data=zlib.decompress(raw)
        elif typ==0x80000006: data=bz2.decompress(raw)
        elif typ==0x80000007: raise SystemExit('lzfse unsupported')
        else: raise SystemExit('unknown chunk type %x'%typ)
        o.seek((sectnum+sec)*512); o.write(data)
o.close(); print('done', os.path.getsize(out))
