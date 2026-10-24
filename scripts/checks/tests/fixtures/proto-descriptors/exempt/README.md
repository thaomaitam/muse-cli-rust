# Fixture: exempt (proto-descriptors)

Miniature descriptors (NoiseTransportFrame, ServiceRequest, ServiceType) + miniature proto.rs. pass: all tags/values match. fail: ServiceRequest.payload tag changed to 3 in proto.rs. exempt: ServiceRequest.payload omitted from proto.rs and listed in exempt.txt with a reason.
