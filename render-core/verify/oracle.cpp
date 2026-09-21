// Golden oracle: the EXACT ft.cpp CAlphaBlend math (init + rconv1 + conv1/conv2
// + CAlphaBlendColorOne::doAB), identity tune table, greyscale solid blend.
#include <cstdio>
#include <vector>
#include <cmath>
#include <string>
using namespace std;
typedef unsigned char BYTE;
static const int BASE = 0x4000;

struct AB {
    vector<int> alphatbl, tbl1, tunetbl;
    vector<BYTE> tbl2;
    AB(): alphatbl(256), tbl1(257), tunetbl(256), tbl2(256*16+1) {}
    int  conv1(BYTE n) { return tbl1[n]; }
    BYTE conv2(int n)  { return tbl2[n / (BASE * BASE / ((int)tbl2.size() - 1))]; }
    BYTE rconv1(int n) {
        int pos = 0x80; int i = pos >> 1;
        while (i > 0) { if (n >= tbl1[pos]) pos += i; else pos -= i; i >>= 1; }
        if (n >= tbl1[pos]) ++pos;
        return (BYTE)(pos - 1);
    }
    void init(float gamma, float weight, float contrast, int mode) {
        int i; float temp, alpha;
        for (i = 0; i < 256; ++i) {
            temp = pow((1.0f/255.0f) * i, 1.0f/weight);
            if (temp < 0.5f) alpha = pow(temp*2, contrast)/2.0f;
            else             alpha = 1.0f - pow((1.0f-temp)*2, contrast)/2.0f;
            alphatbl[i] = (int)(alpha * BASE);
            if (mode < 0) temp = (1.0f/255.0f) * i;
            else if (mode == 1) { if (i<=10) temp=(float)i/(12.92f*255.0f); else temp=pow(((1.0f/255.0f)*i+0.055f)/1.055f,2.4f); }
            else if (mode == 2) { if (i<=10) temp=((float)i/(12.92f*255.0f)+(float)i/255.0f)/2; else temp=(pow(((1.0f/255.0f)*i+0.055f)/1.055f,2.4f)+(float)i/255.0f)/2; }
            else temp = pow((1.0f/255.0f)*i, gamma);
            tbl1[i] = (int)(temp * BASE);
        }
        tbl1[i] = BASE;
        for (i = 0; i <= (int)tbl2.size()-1; ++i) tbl2[i] = rconv1(i * (BASE/((int)tbl2.size()-1)));
        for (i = 0; i < 256; ++i) { int v = alphatbl[i]; if (v<0) v=0; if (v>BASE) v=BASE; tunetbl[i]=v; }
    }
    BYTE doAB(BYTE bg, BYTE fg, int cov) {
        int a = tunetbl[cov];
        int temp_fg = conv1(fg);
        return a ? conv2(conv1(bg)*(BASE-a) + temp_fg*a) : bg;
    }
};
int main() {
    struct Case { const char* tag; float g,w,c; int m; } cases[] = {
        {"g125",1.25f,1.0f,1.0f,0},{"linear",1.0f,1.0f,1.0f,-1},{"g13",1.30f,1.0f,1.0f,0},
        {"w115c14",1.25f,1.15f,1.4f,0},{"srgb",1.0f,1.0f,1.0f,1},{"mode2",1.0f,1.0f,1.0f,2} };
    for (auto& cs : cases) {
        AB ab; ab.init(cs.g, cs.w, cs.c, cs.m);
        string fn = string("cpp-") + cs.tag + ".txt";
        FILE* f = fopen(fn.c_str(), "w");
        for (int c = 0; c < 256; ++c) { fprintf(f, "%d", ab.doAB(255,0,c)); if (c<255) fputc(' ', f); }
        fclose(f);
    }
    printf("dumped cpp-*.txt\n");
    {
        AB ab; ab.init(1.25f, 1.0f, 1.0f, 0);
        FILE* f = fopen("lcd-cpp.txt", "w");
        int bgs[3][3] = {{255,255,255},{128,128,128},{200,100,50}};
        int covs[6] = {0,1,64,128,200,255};
        for (int aamode = 2; aamode <= 3; ++aamode)
        for (int bi = 0; bi < 3; ++bi)
        for (int a = 0; a < 6; ++a) for (int b2 = 0; b2 < 6; ++b2) for (int c = 0; c < 6; ++c) {
            int p0=covs[a], p1=covs[b2], p2=covs[c];
            int br=bgs[bi][0], bg=bgs[bi][1], bb=bgs[bi][2];
            int alphaR,alphaG,alphaB;
            if (aamode==2||aamode==4){alphaR=p0;alphaG=p1;alphaB=p2;} else {alphaR=p2;alphaG=p1;alphaB=p0;}
            int R=ab.doAB((BYTE)br,0,alphaB);
            int G=ab.doAB((BYTE)bg,0,alphaG);
            int B=ab.doAB((BYTE)bb,0,alphaR);
            fprintf(f,"%d,%d,%d ",R,G,B);
        }
        fclose(f);
        printf("dumped lcd-cpp.txt\n");
    }
}
