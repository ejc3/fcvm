package dev.fcvm.probe;

import android.app.Activity;
import android.opengl.GLES30;
import android.opengl.GLSurfaceView;
import android.os.Bundle;
import android.os.SystemClock;
import android.util.Log;
import javax.microedition.khronos.egl.EGLConfig;
import javax.microedition.khronos.opengles.GL10;

/** Draws a lit field of instanced cubes as fast as it can and logs the GL renderer and the fps. */
public class MainActivity extends Activity {
    static final String TAG = "PHONEPROBE";
    GLSurfaceView view;

    @Override protected void onCreate(Bundle b) {
        super.onCreate(b);
        view = new GLSurfaceView(this);
        view.setEGLContextClientVersion(3);
        view.setRenderer(new R());
        view.setRenderMode(GLSurfaceView.RENDERMODE_CONTINUOUSLY);
        setContentView(view);
    }
    @Override protected void onPause() { super.onPause(); view.onPause(); }
    @Override protected void onResume() { super.onResume(); view.onResume(); }

    static final String VS =
        "#version 300 es\n" +
        "layout(location=0) in vec3 p; layout(location=1) in vec3 n;\n" +
        "uniform float t; uniform float aspect; uniform int side;\n" +
        "out vec3 vn; out vec3 vc;\n" +
        "void main(){ int i=gl_InstanceID; float x=float(i%side)-float(side)/2.0; float z=float(i/side);\n" +
        " float h=sin(x*0.35+t*2.0)*cos(z*0.27+t*1.3)*2.0;\n" +
        " vec3 w=p*0.4+vec3(x,h,-z-4.0);\n" +
        " float c=cos(t*0.2), s=sin(t*0.2); w.xz=mat2(c,-s,s,c)*w.xz; w.y-=3.0;\n" +
        " vn=n; vc=0.5+0.5*vec3(sin(x*0.3),sin(z*0.2+1.0),sin(h+2.0));\n" +
        " gl_Position=vec4(w.x/aspect, w.y, -w.z*0.02, -w.z*0.5+0.5); }\n";
    static final String FS =
        "#version 300 es\nprecision highp float; in vec3 vn; in vec3 vc; out vec4 o;\n" +
        "void main(){ vec3 l=normalize(vec3(0.4,0.8,0.5)); float d=max(dot(normalize(vn),l),0.0);\n" +
        " vec3 col=vc*(0.25+0.75*d); for(int k=0;k<24;k++){ col=col*0.999+0.001*sin(col*float(k)); }\n" +
        " o=vec4(col,1.0);} \n";

    static class R implements GLSurfaceView.Renderer {
        int prog, side = 160; long t0, last; int frames; float aspect = 1f;
        int ut, ua, us;
        static int sh(int type, String src) {
            int s = GLES30.glCreateShader(type); GLES30.glShaderSource(s, src); GLES30.glCompileShader(s);
            int[] ok = new int[1]; GLES30.glGetShaderiv(s, GLES30.GL_COMPILE_STATUS, ok, 0);
            if (ok[0] == 0) Log.e(TAG, "shader: " + GLES30.glGetShaderInfoLog(s));
            return s;
        }
        @Override public void onSurfaceCreated(GL10 gl, EGLConfig c) {
            Log.i(TAG, "renderer=" + GLES30.glGetString(GLES30.GL_RENDERER)
                + " vendor=" + GLES30.glGetString(GLES30.GL_VENDOR)
                + " version=" + GLES30.glGetString(GLES30.GL_VERSION));
            prog = GLES30.glCreateProgram();
            GLES30.glAttachShader(prog, sh(GLES30.GL_VERTEX_SHADER, VS));
            GLES30.glAttachShader(prog, sh(GLES30.GL_FRAGMENT_SHADER, FS));
            GLES30.glLinkProgram(prog);
            ut = GLES30.glGetUniformLocation(prog, "t"); ua = GLES30.glGetUniformLocation(prog, "aspect");
            us = GLES30.glGetUniformLocation(prog, "side");
            float[] v = cube();
            java.nio.FloatBuffer fb = java.nio.ByteBuffer.allocateDirect(v.length * 4)
                .order(java.nio.ByteOrder.nativeOrder()).asFloatBuffer(); fb.put(v).position(0);
            int[] b = new int[1]; GLES30.glGenBuffers(1, b, 0); GLES30.glBindBuffer(GLES30.GL_ARRAY_BUFFER, b[0]);
            GLES30.glBufferData(GLES30.GL_ARRAY_BUFFER, v.length * 4, fb, GLES30.GL_STATIC_DRAW);
            GLES30.glVertexAttribPointer(0, 3, GLES30.GL_FLOAT, false, 24, 0); GLES30.glEnableVertexAttribArray(0);
            GLES30.glVertexAttribPointer(1, 3, GLES30.GL_FLOAT, false, 24, 12); GLES30.glEnableVertexAttribArray(1);
            GLES30.glEnable(GLES30.GL_DEPTH_TEST);
            t0 = last = SystemClock.elapsedRealtime();
        }
        @Override public void onSurfaceChanged(GL10 gl, int w, int h) {
            GLES30.glViewport(0, 0, w, h); aspect = (float) w / h;
            Log.i(TAG, "surface " + w + "x" + h);
        }
        @Override public void onDrawFrame(GL10 gl) {
            long now = SystemClock.elapsedRealtime();
            GLES30.glClearColor(0.05f, 0.07f, 0.1f, 1f);
            GLES30.glClear(GLES30.GL_COLOR_BUFFER_BIT | GLES30.GL_DEPTH_BUFFER_BIT);
            GLES30.glUseProgram(prog);
            GLES30.glUniform1f(ut, (now - t0) / 1000f); GLES30.glUniform1f(ua, aspect); GLES30.glUniform1i(us, side);
            GLES30.glDrawArraysInstanced(GLES30.GL_TRIANGLES, 0, 36, side * side);
            frames++;
            if (now - last >= 250) {
                Log.i(TAG, String.format("fps=%.1f instances=%d", frames * 1000.0 / (now - last), side * side));
                frames = 0; last = now;
            }
        }
        static float[] cube() {
            float[][] f = {{1,0,0},{-1,0,0},{0,1,0},{0,-1,0},{0,0,1},{0,0,-1}};
            float[] out = new float[36 * 6]; int o = 0;
            for (float[] n : f) {
                float[] u = Math.abs(n[1]) > 0 ? new float[]{1,0,0} : new float[]{0,1,0};
                float[] v = {n[1]*u[2]-n[2]*u[1], n[2]*u[0]-n[0]*u[2], n[0]*u[1]-n[1]*u[0]};
                float[][] q = new float[4][3];
                int[][] sg = {{-1,-1},{1,-1},{1,1},{-1,1}};
                for (int k = 0; k < 4; k++) for (int j = 0; j < 3; j++) q[k][j] = n[j] + sg[k][0]*u[j] + sg[k][1]*v[j];
                int[] idx = {0,1,2,0,2,3};
                for (int k : idx) { for (int j = 0; j < 3; j++) out[o++] = q[k][j]; for (int j = 0; j < 3; j++) out[o++] = n[j]; }
            }
            return out;
        }
    }
}
