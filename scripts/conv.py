import struct, zlib, sys
src, dst = sys.argv[1], sys.argv[2]
d = open(src,'rb').read()
off = struct.unpack('<I', d[10:14])[0]; w,h = struct.unpack('<ii', d[18:26]); stride = w*4
rows=[]
for y in range(h-1,-1,-1):
    s = off+y*stride; line = d[s:s+stride]
    rgb = bytearray()
    for x in range(w):
        b,g,r,a = line[x*4:x*4+4]; rgb += bytes((r,g,b))
    rows.append(b'\x00'+bytes(rgb))
raw=b''.join(rows)
def ch(t,data):
    c=struct.pack('>I',len(data))+t+data
    return c+struct.pack('>I', zlib.crc32(t+data)&0xffffffff)
png=b'\x89PNG\r\n\x1a\n'+ch(b'IHDR',struct.pack('>IIBBBBB',w,h,8,2,0,0,0))+ch(b'IDAT',zlib.compress(raw,9))+ch(b'IEND',b'')
open(dst,'wb').write(png); print('ok',dst,w,h)
