// One triangle over the whole image; the sky is drawn per whole-image pixel.
void main() {
    vec2 p = vec2(gl_VertexID == 1 ? 3.0 : -1.0, gl_VertexID == 2 ? 3.0 : -1.0);
    gl_Position = vec4(p, 1.0, 1.0);
}
