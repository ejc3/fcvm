// A sign with a viewer's word, carried by a robot (FrameState.Sign). (u, v) on the board's
// front; elsewhere u is 10 + the material.
layout(location = 0) in vec3 a_pos;
layout(location = 1) in vec3 a_nrm;
layout(location = 2) in vec2 a_uv;
uniform vec3 u_signAt;
#ifndef SHADOW
out vec3 v_world;
out vec3 v_nrm;
out vec2 v_uv;
out float v_mat;
out vec4 v_ls;
#endif

void main() {
    vec3 p = a_pos;
    p.x += sin(u_tw * 4.0 + u_signAt.z) * 0.04 * (p.y - 0.85);
    vec3 world = vec3(u_signAt.x, 0.0, u_signAt.y) + p;
#ifdef SHADOW
    gl_Position = u_lightViewProj * vec4(world, 1.0);
#else
    gl_Position = u_viewProj * vec4(world, 1.0);
    v_world = world;
    v_nrm = a_nrm;
    v_uv = a_uv;
    v_mat = a_uv.x >= 9.5 ? a_uv.x - 10.0 : -1.0;
    v_ls = u_lightViewProj * vec4(world + a_nrm * 0.04, 1.0);
#endif
}
