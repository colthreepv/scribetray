# Renders docs/media/streaming-speed.png from latencies measured with
# examples/openai-stream-smoke (batch) and examples/openai-stream-probe (stream).
# Run from the repo root: uv run --with pillow python scripts/render-speed-chart.py
from PIL import Image, ImageDraw, ImageFont, ImageFilter, ImageChops
S=2
W,H=1280*S,520*S
img=Image.new("RGB",(W,H),(19,20,25))
glow=Image.new("RGB",(W,H),(0,0,0)); g=ImageDraw.Draw(glow)
g.ellipse((W*0.62,H*0.05,W*1.05,H*0.95),fill=(40,12,15))
glow=glow.filter(ImageFilter.GaussianBlur(140*S))
img=ImageChops.add(img,glow)
d=ImageDraw.Draw(img)
F="C:/Windows/Fonts/"
def f(n,s): return ImageFont.truetype(F+n,s*S)
title=f("seguisb.ttf",40); sub=f("segoeui.ttf",22); lab=f("segoeui.ttf",22); grp=f("seguisb.ttf",22); val=f("seguisb.ttf",23); small=f("segoeui.ttf",17)
WHITE=(240,240,243); GRAY=(150,152,160); DIM=(90,92,100); RED=(229,72,77); BAR=(60,62,70); GRID=(36,38,45)
x0=80*S
d.text((x0,50*S),"Text in a quarter second",font=title,fill=WHITE)
d.text((x0,106*S),"Time from stop to text with the OpenAI provider. Streaming stays flat as dictation grows.",font=sub,fill=GRAY)
left=250*S; right=1130*S; maxs=4.0
def X(s): return left+(right-left)*s/maxs
top=170*S; bottom=455*S
for t in range(0,5):
    x=X(t); d.line((x,top,x,bottom-22*S),fill=GRID,width=S)
    d.text((x,bottom-10*S),f"{t} s",font=small,fill=DIM,anchor="mt")
groups=[("64 s of speech",1.8,0.25),("192 s of speech",3.4,0.24)]
y=top+4*S
h=30*S
for name,batch,stream in groups:
    d.text((x0,y),name,font=grp,fill=WHITE,anchor="lt")
    y+=36*S
    for kind,v,col,tc in (("Batch",batch,BAR,GRAY),("Streaming",stream,RED,WHITE)):
        d.text((x0,y+h/2),kind,font=lab,fill=tc,anchor="lm")
        d.rounded_rectangle((left,y,max(X(v),left+h),y+h),radius=7*S,fill=col)
        d.text((max(X(v),left+h)+14*S,y+h/2),f"{v:.2f} s",font=val,fill=tc,anchor="lm")
        y+=h+10*S
    y+=26*S
img=img.resize((W//S,H//S),Image.LANCZOS)
img.save("docs/media/streaming-speed.png",optimize=True)

