// The robots: one instanced draw, every pose computed here from the robot's table entry.
layout(location = 0) in vec3 a_pos;
layout(location = 1) in vec3 a_nrm;
layout(location = 2) in vec2 a_part;   // part, material
layout(location = 3) in vec4 i_a;
layout(location = 4) in vec4 i_b;
#ifndef SHADOW
out vec3 v_world;
out vec3 v_nrm;
out vec3 v_col;
out float v_emit;
out float v_spec;
out vec4 v_ls;
#endif

void limb(vec3 pivot, float swing, float raise, inout vec3 p, inout vec3 n) {
    p = rotZ(rotX(p - pivot, swing), raise) + pivot;
    n = rotZ(rotX(n, swing), raise);
}

void main() {
    Pose P = robotPose(i_a, i_b);
    vec3 p = a_pos;
    vec3 n = a_nrm;
    int part = int(a_part.x + 0.5);
    if (part == 1) {
        limb(vec3(0.0, 0.66, 0.0), P.headNod, P.headTilt, p, n);
    } else if (part == 2) {
        limb(vec3(0.42, 0.56, 0.0), P.armSwingL, P.armRaiseL, p, n);
    } else if (part == 3) {
        limb(vec3(-0.42, 0.56, 0.0), P.armSwingR, -P.armRaiseR, p, n);
    } else if (part == 4) {
        limb(vec3(0.14, 0.2, 0.0), P.legL, 0.0, p, n);
    } else if (part == 5) {
        limb(vec3(-0.14, 0.2, 0.0), P.legR, 0.0, p, n);
    }
    float s = i_b.y;
    p.y *= P.squash;
    vec3 world = P.root + vec3(0.0, P.lift, 0.0) + rotY(p * s, P.yaw);
    vec3 wn = normalize(rotY(n, P.yaw));
#ifdef SHADOW
    gl_Position = u_lightViewProj * vec4(world, 1.0);
#else
    gl_Position = u_viewProj * vec4(world, 1.0);
    v_world = world;
    v_nrm = wn;
    float mat = a_part.y;
    v_col = mat > 1.5 ? (mat > 2.5 ? palette(9.0) : palette(8.0)) : palette(i_b.x);
    v_emit = mat > 1.5 && mat < 2.5 ? u_night * 1.6 : 0.0;
    if (mat > 1.5 && mat < 2.5 && u_night > 0.5) v_col = vec3(1.0, 0.95, 0.7);
    v_spec = 0.45;
    v_ls = u_lightViewProj * vec4(world + wn * 0.04, 1.0);
#endif
}
