in vec3 v_world;
in vec3 v_nrm;
in vec3 v_col;
in float v_emit;
in float v_spec;
in vec4 v_ls;
out vec4 o;

void main() {
    vec3 n = normalize(v_nrm);
    vec3 c = shade(v_col, n, v_world, v_ls, v_spec) + v_col * v_emit;
    o = vec4(finish(c, v_world), 1.0);
}
