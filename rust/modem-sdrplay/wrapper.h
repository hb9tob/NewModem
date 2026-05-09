/* bindgen entry point — pulls in the entire SDRplay API 3.x surface
 * from the user-installed SDK (typically /usr/local/include after
 * running SDRplay_RSP_API-Linux-ARM64-3.X.run as root).
 *
 * Header path is resolved by build.rs which adds -I/usr/local/include
 * (or whatever SDRPLAY_API_INCLUDE_DIR points at). The single root
 * header transitively includes the per-RSP variants (rsp1a, rsp2,
 * rspDuo, rspDx) plus the shared dev/tuner/control/callback headers.
 */
#include <sdrplay_api.h>
