use super::camera::Camera;
use serde::Serialize;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

pub(super) const DEADLINE: Duration = Duration::from_secs(10);
#[derive(Debug, Clone)]
pub(super) struct Failure {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}
impl Failure {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub(super) struct CameraRevision {
    pub camera_revision: u64,
    pub camera: Camera,
}
#[derive(Debug, Clone, Serialize)]
pub(super) struct Submission {
    pub frame_id: u64,
    pub camera_revision: u64,
    pub camera: Camera,
}
#[derive(Debug, Clone, Serialize)]
pub(super) struct Outcome {
    pub camera_revision: u64,
    pub status: &'static str,
}
struct Request {
    camera: CameraRevision,
    deadline: Instant,
}
pub(super) struct CaptureRequest {
    pub id: u64,
    pub deadline: Instant,
    pub tx: oneshot::Sender<Result<RawFrame, Failure>>,
}
pub(super) struct RawFrame {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub bgra: bool,
    pub proof: Submission,
    pub deadline: Instant,
}
pub(super) struct Model {
    pub active: CameraRevision,
    pub requested: CameraRevision,
    pub submitted: Option<Submission>,
    pub captured: Option<Submission>,
    pub phase: &'static str,
    pub terminal_error: Option<String>,
    pub shutdown: bool,
    pub viewport: [u32; 2],
    pub radius: f32,
    next_revision: u64,
    next_frame: u64,
    next_capture: u64,
    pending: VecDeque<Request>,
    pub outcomes: VecDeque<Outcome>,
    pub capture_id: Option<u64>,
    pub capture_pending: Option<CaptureRequest>,
}
impl Model {
    pub fn new(camera: Camera) -> Self {
        let active = CameraRevision {
            camera_revision: 0,
            camera,
        };
        Self {
            requested: active.clone(),
            active,
            submitted: None,
            captured: None,
            phase: "initializing",
            terminal_error: None,
            shutdown: false,
            viewport: [0, 0],
            radius: 1.,
            next_revision: 1,
            next_frame: 1,
            next_capture: 1,
            pending: VecDeque::new(),
            outcomes: VecDeque::new(),
            capture_id: None,
            capture_pending: None,
        }
    }
    fn outcome(&mut self, revision: u64, status: &'static str) {
        if let Some(o) = self
            .outcomes
            .iter_mut()
            .find(|o| o.camera_revision == revision)
        {
            o.status = status;
        } else {
            self.outcomes.push_back(Outcome {
                camera_revision: revision,
                status,
            });
        }
        while self.outcomes.len() > 32 {
            self.outcomes.pop_front();
        }
    }
    fn revision(&mut self, camera: Camera) -> Result<CameraRevision, Failure> {
        let revision = self.next_revision;
        self.next_revision = revision
            .checked_add(1)
            .ok_or_else(|| Failure::new(503, "sequence_exhausted", "camera sequence exhausted"))?;
        Ok(CameraRevision {
            camera_revision: revision,
            camera,
        })
    }
    pub fn queue_camera(&mut self, camera: Camera, now: Instant) -> Result<u64, Failure> {
        if self.shutdown {
            return Err(Failure::new(
                503,
                "shutting_down",
                "preview is shutting down",
            ));
        }
        camera
            .matrices(self.aspect(), self.radius)
            .map_err(|e| Failure::new(400, "invalid_camera", e.to_string()))?;
        self.expire_requests(now);
        if self.pending.len() >= 8 {
            return Err(Failure::new(
                429,
                "camera_queue_full",
                "camera queue is full",
            ));
        }
        let camera = self.revision(camera)?;
        let revision = camera.camera_revision;
        self.requested = camera.clone();
        self.outcome(revision, "queued");
        self.pending.push_back(Request {
            camera,
            deadline: now + DEADLINE,
        });
        Ok(revision)
    }
    pub fn expire_requests(&mut self, now: Instant) {
        let mut remaining = VecDeque::new();
        while let Some(r) = self.pending.pop_front() {
            if now >= r.deadline {
                self.outcome(r.camera.camera_revision, "expired");
            } else {
                remaining.push_back(r);
            }
        }
        self.pending = remaining;
    }
    fn aspect(&self) -> f32 {
        if self.viewport.contains(&0) {
            4. / 3.
        } else {
            self.viewport[0] as f32 / self.viewport[1] as f32
        }
    }
    fn supersede_active(&mut self) {
        if self
            .outcomes
            .iter()
            .any(|o| o.camera_revision == self.active.camera_revision && o.status == "active")
        {
            self.outcome(self.active.camera_revision, "superseded");
        }
    }
    pub fn apply_requests(&mut self, now: Instant) {
        self.expire_requests(now);
        let latest = self.pending.pop_back();
        while let Some(r) = self.pending.pop_front() {
            self.outcome(r.camera.camera_revision, "superseded");
        }
        if let Some(r) = latest {
            if r.camera
                .camera
                .matrices(self.aspect(), self.radius)
                .is_err()
            {
                self.outcome(r.camera.camera_revision, "failed");
            } else {
                self.supersede_active();
                self.active = r.camera;
                self.outcome(self.active.camera_revision, "active");
            }
        }
    }
    pub fn mouse_camera(&mut self, camera: Camera) {
        if self.shutdown {
            return;
        }
        while let Some(r) = self.pending.pop_front() {
            self.outcome(r.camera.camera_revision, "superseded");
        }
        match self.revision(camera) {
            Ok(camera) => {
                self.supersede_active();
                self.requested = camera.clone();
                self.active = camera;
                self.outcome(self.active.camera_revision, "active");
            }
            Err(e) => self.fail(e.message),
        }
    }
    pub fn publish_submission(&mut self) -> Result<Option<Submission>, Failure> {
        if self.shutdown {
            Ok(None)
        } else {
            self.submitted().map(Some)
        }
    }
    pub fn submitted(&mut self) -> Result<Submission, Failure> {
        let proof = Submission {
            frame_id: self.next_frame,
            camera_revision: self.active.camera_revision,
            camera: self.active.camera.clone(),
        };
        self.next_frame = self
            .next_frame
            .checked_add(1)
            .ok_or_else(|| Failure::new(503, "sequence_exhausted", "frame sequence exhausted"))?;
        self.submitted = Some(proof.clone());
        self.outcome(proof.camera_revision, "submitted");
        Ok(proof)
    }
    pub fn complete_capture(
        &mut self,
        id: u64,
        viewport: [u32; 2],
        proof: Submission,
    ) -> Result<(), Failure> {
        if self.shutdown {
            return Err(Failure::new(
                503,
                "shutting_down",
                "preview is shutting down",
            ));
        }
        if self.phase != "ready" || self.viewport.contains(&0) {
            return Err(Failure::new(
                503,
                "capture_unavailable",
                "preview window became unavailable before image publication",
            ));
        }
        if self.viewport != viewport {
            return Err(Failure::new(
                503,
                "capture_unavailable",
                "preview window dimensions changed before image publication",
            ));
        }
        if self.capture_id != Some(id) {
            return Err(Failure::new(
                503,
                "capture_request_unavailable",
                "capture request is no longer current",
            ));
        }
        self.captured = Some(proof);
        Ok(())
    }
    pub fn window_changed(&mut self, viewport: [u32; 2], occluded: bool) {
        self.viewport = viewport;
        if !self.shutdown {
            self.phase = if occluded {
                "occluded"
            } else if viewport.contains(&0) {
                "minimized"
            } else {
                "ready"
            };
        }
        let unavailable = self.phase != "ready" || viewport.contains(&0);
        let oversized = viewport.iter().any(|&n| n > 2048);
        if unavailable || oversized {
            if let Some(r) = self.capture_pending.take() {
                let failure = if oversized && !unavailable {
                    Failure::new(
                        413,
                        "capture_size_limit",
                        "capture exceeds 2048 pixels per dimension",
                    )
                } else {
                    Failure::new(
                        503,
                        "capture_unavailable",
                        "preview window became unavailable",
                    )
                };
                let _ = r.tx.send(Err(failure));
            }
        }
    }
    pub fn reserve_capture(
        &mut self,
        now: Instant,
    ) -> Result<(u64, oneshot::Receiver<Result<RawFrame, Failure>>), Failure> {
        if self.shutdown || self.phase != "ready" || self.viewport.contains(&0) {
            return Err(Failure::new(
                503,
                "capture_unavailable",
                "preview window is not available",
            ));
        }
        if self.viewport.iter().any(|&n| n > 2048) {
            return Err(Failure::new(
                413,
                "capture_size_limit",
                "capture is limited to 2048 pixels per dimension",
            ));
        }
        if self.capture_id.is_some() {
            return Err(Failure::new(
                409,
                "capture_busy",
                "another capture is in flight",
            ));
        }
        let id = self.next_capture;
        self.next_capture = id
            .checked_add(1)
            .ok_or_else(|| Failure::new(503, "sequence_exhausted", "capture sequence exhausted"))?;
        let (tx, rx) = oneshot::channel();
        self.capture_id = Some(id);
        self.capture_pending = Some(CaptureRequest {
            id,
            deadline: now + DEADLINE,
            tx,
        });
        Ok((id, rx))
    }
    pub fn release_capture(&mut self, id: u64) {
        if self.capture_id == Some(id) {
            self.capture_id = None;
            self.capture_pending = None;
        }
    }
    pub fn stop(&mut self) {
        self.shutdown = true;
        if self.terminal_error.is_none() {
            self.phase = "stopping";
        }
        while let Some(r) = self.pending.pop_front() {
            self.outcome(r.camera.camera_revision, "failed");
        }
        if self
            .outcomes
            .iter()
            .any(|o| o.camera_revision == self.active.camera_revision && o.status == "active")
        {
            self.outcome(self.active.camera_revision, "failed");
        }
        if let Some(r) = self.capture_pending.take() {
            let _ = r.tx.send(Err(Failure::new(
                503,
                "shutting_down",
                "preview is shutting down",
            )));
        }
    }
    pub fn fail(&mut self, message: String) {
        self.terminal_error = Some(message);
        self.phase = "failed";
        self.stop();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn camera() -> Camera {
        Camera {
            eye: [0., -4., 2.],
            target: [0.; 3],
            fov_degrees: 60.,
        }
    }
    #[test]
    fn queued_cameras_do_not_claim_submission_and_competition_is_explicit() {
        let mut m = Model::new(camera());
        let now = Instant::now();
        let a = m.queue_camera(camera(), now).unwrap();
        let mut c = camera();
        c.eye[0] = 2.;
        let b = m.queue_camera(c.clone(), now).unwrap();
        assert!(m.submitted.is_none());
        assert_eq!(m.active.camera_revision, 0);
        m.apply_requests(now);
        assert_eq!(m.active.camera, c);
        assert_eq!(
            m.outcomes
                .iter()
                .find(|o| o.camera_revision == a)
                .unwrap()
                .status,
            "superseded"
        );
        assert_eq!(
            m.outcomes
                .iter()
                .find(|o| o.camera_revision == b)
                .unwrap()
                .status,
            "active"
        );
        let proof = m.submitted().unwrap();
        assert_eq!(proof.camera_revision, b);
        assert_eq!(
            m.outcomes
                .iter()
                .find(|o| o.camera_revision == b)
                .unwrap()
                .status,
            "submitted"
        );
    }
    #[test]
    fn camera_queue_is_bounded_and_expired_requests_never_apply() {
        let mut m = Model::new(camera());
        let now = Instant::now();
        for _ in 0..8 {
            m.queue_camera(camera(), now).unwrap();
        }
        assert_eq!(m.queue_camera(camera(), now).unwrap_err().status, 429);
        m.apply_requests(now + DEADLINE + Duration::from_millis(1));
        assert_eq!(m.active.camera_revision, 0);
        assert!(m.outcomes.iter().all(|o| o.status == "expired"));
    }
    #[test]
    fn mouse_changes_supersede_pending_api_work_and_are_the_next_submission() {
        let mut m = Model::new(camera());
        let request = m.queue_camera(camera(), Instant::now()).unwrap();
        let mut c = camera();
        c.eye[1] = -8.;
        m.mouse_camera(c.clone());
        m.apply_requests(Instant::now());
        assert_eq!(m.submitted().unwrap().camera, c);
        assert_eq!(
            m.outcomes
                .iter()
                .find(|o| o.camera_revision == request)
                .unwrap()
                .status,
            "superseded"
        );
    }
    #[test]
    fn capture_is_unavailable_until_ready_single_flight_and_identity_checked() {
        let mut m = Model::new(camera());
        let now = Instant::now();
        assert!(m.reserve_capture(now).is_err());
        m.phase = "ready";
        m.viewport = [640, 480];
        let (a, _) = m.reserve_capture(now).unwrap();
        assert_eq!(m.reserve_capture(now).err().unwrap().status, 409);
        m.release_capture(a + 1);
        assert_eq!(m.capture_id, Some(a));
        m.release_capture(a);
        assert!(m.reserve_capture(now).is_ok());
    }
    #[test]
    fn shutdown_prevents_new_work_and_capture_success() {
        let mut m = Model::new(camera());
        m.phase = "ready";
        m.viewport = [640, 480];
        m.stop();
        assert!(m.queue_camera(camera(), Instant::now()).is_err());
        assert!(m.reserve_capture(Instant::now()).is_err());
    }
    #[test]
    fn overflowing_camera_is_rejected_before_queue_and_does_not_stop_scene() {
        let mut m = Model::new(camera());
        m.phase = "ready";
        m.viewport = [800, 600];
        m.radius = 16.;
        let extreme = Camera {
            eye: [3e38; 3],
            target: [-3e38; 3],
            fov_degrees: 60.,
        };
        assert!(extreme.validate().is_ok());
        assert_eq!(
            m.queue_camera(extreme, Instant::now()).unwrap_err().status,
            400
        );
        assert_eq!(m.active.camera_revision, 0);
        assert!(m.pending.is_empty());
        assert!(!m.shutdown);
    }
    #[test]
    fn active_admission_has_no_queued_deadline_or_false_submission() {
        let mut m = Model::new(camera());
        let now = Instant::now();
        let id = m.queue_camera(camera(), now).unwrap();
        m.apply_requests(now);
        m.expire_requests(now + Duration::from_secs(11));
        assert_eq!(
            m.outcomes
                .iter()
                .find(|o| o.camera_revision == id)
                .unwrap()
                .status,
            "active"
        );
        assert!(m.submitted.is_none());
        let mut c = camera();
        c.eye[0] = 2.;
        m.mouse_camera(c);
        assert_eq!(
            m.outcomes
                .iter()
                .find(|o| o.camera_revision == id)
                .unwrap()
                .status,
            "superseded"
        );
    }
    #[test]
    fn intentional_stop_after_gpu_submission_does_not_publish_new_proof() {
        let mut m = Model::new(camera());
        let old = m.publish_submission().unwrap().unwrap();
        m.stop();
        assert!(m.publish_submission().unwrap().is_none());
        assert_eq!(m.submitted.as_ref().unwrap().frame_id, old.frame_id);
    }

    #[test]
    fn unavailable_resize_cancels_pending_capture_without_reviving_shutdown() {
        let mut m = Model::new(camera());
        m.window_changed([800, 600], false);
        let (_, mut rx) = m.reserve_capture(Instant::now()).unwrap();
        m.window_changed([0, 0], false);
        assert_eq!(rx.try_recv().unwrap().err().unwrap().status, 503);
        assert_eq!(m.phase, "minimized");
        m.stop();
        m.window_changed([800, 600], false);
        assert_eq!(m.phase, "stopping");
        assert!(m.reserve_capture(Instant::now()).is_err());
    }

    #[test]
    fn completed_readback_cannot_publish_after_late_window_unavailability() {
        for (size, occluded) in [([0, 0], false), ([640, 480], true), ([800, 600], false)] {
            let mut m = Model::new(camera());
            m.window_changed([640, 480], false);
            let (id, _rx) = m.reserve_capture(Instant::now()).unwrap();
            let _taken = m.capture_pending.take().unwrap();
            let proof = m.publish_submission().unwrap().unwrap();
            m.window_changed(size, occluded);
            assert!(m.complete_capture(id, [640, 480], proof).is_err());
            assert!(m.captured.is_none());
        }
    }
    #[test]
    fn capture_completion_checks_ownership_but_accepts_exact_older_camera_proof() {
        let mut m = Model::new(camera());
        m.window_changed([640, 480], false);
        let (id, _rx) = m.reserve_capture(Instant::now()).unwrap();
        let proof = m.publish_submission().unwrap().unwrap();
        assert!(m
            .complete_capture(id + 1, [640, 480], proof.clone())
            .is_err());
        assert!(m.captured.is_none());
        let mut newer = camera();
        newer.eye[0] = 3.;
        m.mouse_camera(newer);
        m.publish_submission().unwrap();
        m.complete_capture(id, [640, 480], proof.clone()).unwrap();
        assert_eq!(m.captured.unwrap().camera_revision, proof.camera_revision);
    }
}
