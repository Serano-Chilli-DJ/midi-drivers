import sys,struct,zlib,os
d=open(sys.argv[1],'rb').read(); out=sys.argv[2]; os.makedirs(out,exist_ok=True)
o=d.rindex(b'ISSetupStream\x00')
sig,num,typ=struct.unpack_from('<14sHI',d,o); o+=46
print('files',num,'type',typ)
MAGIC=[0x13,0x35,0x86,0x07]
def dec(b,k): return (~(k ^ (((b<<4)&0xff)|(b>>4))))&0xff
for i in range(num):
    nlen,flags,x3,flen,x5,ul=struct.unpack_from('<IIHIQH',d,o); o+=24
    if typ==4: o+=24
    name=d[o:o+nlen].decode('utf-16le'); o+=nlen
    data=bytearray(d[o:o+flen]); o+=flen
    seed=name.encode('utf-8'); key=[seed[j]^MAGIC[j%4] for j in range(len(seed))]; kl=len(key)
    if flags&4 and flags&6:
        for blk in range(0,len(data),1024):
            for j in range(min(1024,len(data)-blk)):
                data[blk+j]=dec(data[blk+j],key[j%kl])
    if ul:
        try: data=zlib.decompress(bytes(data),-15)
        except Exception as e:
            try: data=zlib.decompress(bytes(data))
            except Exception as e2: print('  inflate failed',e,e2)
    open(os.path.join(out,name),'wb').write(data)
    print('  %s flags=%d len=%d ul=%d -> %d head=%s'%(name,flags,flen,ul,len(data),bytes(data[:8])))
