#!/usr/bin/env python3
from threading import Lock, Thread
class Q:
 def __init__(s): s.used=0;s.active=0;s.epoch=10;s.seen={};s.lock=Lock()
 def admit(s,cost=1,epoch=10,key=None):
  with s.lock:
   if epoch<s.epoch:return 409
   if key in s.seen:return s.seen[key]
   if s.used+cost>2:r=429
   elif s.active>=1:r=503
   else:s.used+=cost;s.active+=1;r=200
   if key:s.seen[key]=r
   return r
q=Q();assert q.admit(key="a")==200
u=q.used;assert q.admit(key="a")==200 and q.used==u
assert q.admit(key="b")==503 and q.used==u
q.active=0;assert q.admit(cost=2,key="c")==429 and q.used==u
assert q.admit(epoch=9,key="d")==409 and q.used==u
q=Q();q.active=0;q.used=0
codes=[]
def hit(i):codes.append(q.admit(key=str(i)))
ts=[Thread(target=hit,args=(i,)) for i in range(20)]
[x.start() for x in ts];[x.join() for x in ts]
assert codes.count(200)==1 and codes.count(503)==19 and q.used==1
print("quota/admission conformance: PASS")
