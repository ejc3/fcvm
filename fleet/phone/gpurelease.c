// Does NVIDIA's user-space driver let go of the GPU when a process tears down its EGL
// contexts and Vulkan devices? Counts /dev/nvidia* descriptors and mappings at each step,
// twice, so the second round also shows the GPU can be taken up again in the same process.
// The measurement behind "Detaching the renderer, measured" in docs/fleet/ARCHITECTURE.md.
// Build where the EGL, GLES and Vulkan headers are installed (libegl-dev, libgles-dev,
// libvulkan-dev) and run it in the phone container:
//   gcc -O2 -Wall -o gpurelease gpurelease.c -ldl
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <dirent.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

static void count(const char *step) {
    int fds = 0, maps = 0;
    char path[300], target[256];
    DIR *d = opendir("/proc/self/fd");
    struct dirent *e;
    while (d && (e = readdir(d))) {
        snprintf(path, sizeof path, "/proc/self/fd/%s", e->d_name);
        ssize_t n = readlink(path, target, sizeof target - 1);
        if (n > 0) { target[n] = 0; if (strstr(target, "/dev/nvidia")) fds++; }
    }
    if (d) closedir(d);
    FILE *f = fopen("/proc/self/maps", "r");
    char line[512];
    while (f && fgets(line, sizeof line, f)) if (strstr(line, "/dev/nvidia")) maps++;
    if (f) fclose(f);
    printf("%-34s nvidia fds=%d mappings=%d\n", step, fds, maps);
    fflush(stdout);
}

#define LOAD(lib, type, name) type name = (type)dlsym(lib, #name)

static int egl_round(int round) {
    void *lib = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!lib) { printf("dlopen libEGL: %s\n", dlerror()); return 1; }
    LOAD(lib, PFNEGLGETDISPLAYPROC, eglGetDisplay);
    LOAD(lib, PFNEGLINITIALIZEPROC, eglInitialize);
    LOAD(lib, PFNEGLBINDAPIPROC, eglBindAPI);
    LOAD(lib, PFNEGLCHOOSECONFIGPROC, eglChooseConfig);
    LOAD(lib, PFNEGLCREATECONTEXTPROC, eglCreateContext);
    LOAD(lib, PFNEGLCREATEPBUFFERSURFACEPROC, eglCreatePbufferSurface);
    LOAD(lib, PFNEGLMAKECURRENTPROC, eglMakeCurrent);
    LOAD(lib, PFNEGLDESTROYSURFACEPROC, eglDestroySurface);
    LOAD(lib, PFNEGLDESTROYCONTEXTPROC, eglDestroyContext);
    LOAD(lib, PFNEGLTERMINATEPROC, eglTerminate);
    LOAD(lib, PFNEGLRELEASETHREADPROC, eglReleaseThread);
    LOAD(lib, PFNEGLGETPROCADDRESSPROC, eglGetProcAddress);
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    EGLint major, minor;
    if (!eglInitialize(dpy, &major, &minor)) { printf("eglInitialize failed\n"); return 1; }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                      EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE};
    EGLConfig cfg; EGLint n;
    eglChooseConfig(dpy, attrs, &cfg, 1, &n);
    EGLint cattrs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, cattrs);
    EGLint sattrs[] = {EGL_WIDTH, 64, EGL_HEIGHT, 64, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, sattrs);
    eglMakeCurrent(dpy, surf, surf, ctx);
    const GLubyte *(*getString)(GLenum) = (void *)eglGetProcAddress("glGetString");
    void (*clear)(GLbitfield) = (void *)eglGetProcAddress("glClear");
    void (*finish)(void) = (void *)eglGetProcAddress("glFinish");
    clear(GL_COLOR_BUFFER_BIT); finish();
    printf("egl round %d: renderer %s\n", round, getString(GL_RENDERER));
    count("  egl: context current, drawn");
    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroySurface(dpy, surf);
    eglDestroyContext(dpy, ctx);
    eglTerminate(dpy);
    eglReleaseThread();
    count("  egl: terminated, thread released");
    dlclose(lib);
    count("  egl: libEGL dlclosed");
    return 0;
}

static int vk_round(int round) {
    void *lib = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!lib) { printf("dlopen libvulkan: %s\n", dlerror()); return 1; }
    LOAD(lib, PFN_vkCreateInstance, vkCreateInstance);
    LOAD(lib, PFN_vkEnumeratePhysicalDevices, vkEnumeratePhysicalDevices);
    LOAD(lib, PFN_vkGetPhysicalDeviceProperties, vkGetPhysicalDeviceProperties);
    LOAD(lib, PFN_vkCreateDevice, vkCreateDevice);
    LOAD(lib, PFN_vkDestroyDevice, vkDestroyDevice);
    LOAD(lib, PFN_vkDestroyInstance, vkDestroyInstance);
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    VkInstance inst;
    if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) { printf("vkCreateInstance failed\n"); return 1; }
    uint32_t count1 = 1; VkPhysicalDevice pd;
    vkEnumeratePhysicalDevices(inst, &count1, &pd);
    VkPhysicalDeviceProperties props; vkGetPhysicalDeviceProperties(pd, &props);
    float prio = 1.0f;
    VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &prio};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &q};
    VkDevice dev;
    if (vkCreateDevice(pd, &dci, NULL, &dev) != VK_SUCCESS) { printf("vkCreateDevice failed\n"); return 1; }
    printf("vk round %d: device %s\n", round, props.deviceName);
    count("  vk: device created");
    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    count("  vk: device and instance destroyed");
    dlclose(lib);
    count("  vk: libvulkan dlclosed");
    return 0;
}

int main(void) {
    count("start");
    for (int r = 1; r <= 2; r++) {
        if (egl_round(r) || vk_round(r)) return 1;
    }
    return 0;
}
