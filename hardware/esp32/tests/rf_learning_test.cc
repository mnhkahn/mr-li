#include "rf_learning.h"
#include <cassert>
#include <iostream>
std::vector<int> Frame(int key) {
    std::vector<int> p;
    for (int i = 0; i < 24; ++i) { p.push_back((key & (1 << (i % 8))) ? 1000 : 350); p.push_back(-350); }
    p.back() = -10000;
    return p;
}
int main() {
    auto a = Frame(1), b = Frame(2);
    assert(curtain::SameFrame(a,a));
    assert(!curtain::SameFrame(a,b));
    auto jitter=a; jitter[0]+=40; assert(curtain::SameFrame(a,jitter));
    auto raw=a;raw.insert(raw.end(),a.begin(),a.end());raw.push_back(350);
    std::vector<int64_t> ends;
    auto frames=curtain::SplitFrames(raw,6000,&ends);
    assert(frames.size()==2 && ends.size()==2 && ends[1]>ends[0]);
    auto clipped=a;clipped.back()=-30000;
    assert(curtain::SplitFrames(clipped,6000).empty());
    curtain::FrameLearner held;
    for (int i=0;i<100;++i) assert(!held.Feed(a,100000+i*50000));
    assert(held.Progress().bursts==1);
    assert(!held.Feed(a,6000000));
    assert(held.Feed(a,7000000));
    curtain::FrameLearner rolling;
    assert(!rolling.Feed(Frame(1),100000));
    assert(!rolling.Feed(Frame(2),1100000));
    assert(!rolling.Feed(Frame(4),2100000));
    std::vector<int> long_frame;
    for(int i=0;i<256;++i){long_frame.push_back(100);long_frame.push_back(-100);}
    curtain::Validate(long_frame);
    bool failed=false;long_frame.push_back(100);long_frame.push_back(-100);
    try{curtain::Validate(long_frame);}catch(...){failed=true;}assert(failed);
    std::cout << "RF framing, hold-vs-three-press and rolling-frame rejection passed\n";
}
