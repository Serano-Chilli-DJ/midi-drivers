import sys, struct, zlib, os, re, io, gzip, subprocess
import xml.etree.ElementTree as ET
img=sys.argv[1]; outdir=sys.argv[2]
d=open(img,'rb').read()
for n,m in enumerate(re.finditer(rb'xar!', d)):
    off=m.start()
    magic,hsize,ver,toclen_c,toclen_u,cksum=struct.unpack('>4sHHQQI',d[off:off+28])
    if hsize!=28 or ver!=1: continue
    toc=zlib.decompress(d[off+hsize:off+hsize+toclen_c])
    heap=off+hsize+toclen_c
    root=ET.fromstring(toc)
    base=os.path.join(outdir,'xar%d'%n); os.makedirs(base,exist_ok=True)
    open(base+'.toc.xml','wb').write(toc)
    print('xar at',off)
    def walk(el,path):
        for f in el.findall('file'):
            name=f.findtext('name'); typ=f.findtext('type'); p=os.path.join(path,name)
            if typ=='directory':
                os.makedirs(os.path.join(base,p),exist_ok=True); walk(f,p)
            else:
                data=f.find('data')
                if data is None: continue
                o=int(data.findtext('offset')); l=int(data.findtext('length')); enc=data.find('encoding').get('style')
                raw=d[heap+o:heap+o+l]
                if 'gzip' in enc: raw=zlib.decompress(raw)
                elif 'bzip2' in enc: import bz2; raw=bz2.decompress(raw)
                os.makedirs(os.path.join(base,path),exist_ok=True)
                open(os.path.join(base,p),'wb').write(raw); print('  ',p,len(raw))
    walk(root.find('toc'),'')
