import sys; sys.path.insert(0,'/tmp/claude-1000/-home-claude-dev-ns7ii/867fd84c-421a-4748-9f4a-87f68b26b72c/scratchpad/pylib')
import lief, capstone, re, bisect
path=sys.argv[1]; names=sys.argv[2:]
k=lief.parse(path)
syms=sorted([(s.value,s.name) for s in k.symbols if s.value and s.name])
addrs=[v for v,_ in syms]
text=[s for s in k.sections if s.name=='__text'][0]
data=bytes(text.content); base=text.virtual_address
cs=[s for s in k.sections if s.name=='__cstring'][0]; csd=bytes(cs.content); csb=cs.virtual_address
md=capstone.Cs(capstone.CS_ARCH_X86,capstone.CS_MODE_64); md.detail=True
def symat(a):
    i=bisect.bisect_right(addrs,a)-1
    return syms[i][1] if i>=0 and a-syms[i][0]<0x4000 else None
def strat(a):
    if csb<=a<csb+len(csd):
        e=csd.index(b'\0',a-csb); return csd[a-csb:e].decode('latin1',errors='replace')
    return None
for name in names:
    m=[(v,n) for v,n in syms if re.search(name,n)]
    for v,n in m:
        i=bisect.bisect_right(addrs,v); end=addrs[i] if i<len(addrs) else base+len(data)
        print('=========',hex(v),n,'len',end-v)
        for ins in md.disasm(data[v-base:end-base],v):
            extra=''
            if ins.mnemonic=='call' or ins.mnemonic.startswith('j'):
                try: t=int(ins.op_str,16); s=symat(t); extra='  ; '+s if s else ''
                except: pass
            if ins.mnemonic=='lea' and 'rip' in ins.op_str:
                t=ins.address+ins.size+ins.disp; s=strat(t) or symat(t); extra='  ; '+repr(s) if s else ''
            print('%08x  %-7s %s%s'%(ins.address,ins.mnemonic,ins.op_str,extra))
