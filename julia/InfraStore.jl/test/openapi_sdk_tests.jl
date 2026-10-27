# The power-openapi-models Julia types decode what the export writes, so a
# second implementation of the contract reads this one's documents.
@testset "the Julia SDK decodes the export" begin
    store = Store(in_memory=true)
    add_time_series!(
        store, 1, "Generator", Component,
        SingleTimeSeries(DateTime(2030, 1, 1), Hour(1), collect(1.0:4.0), "load"),
    )
    add_supplemental_attribute_association!(
        store, SupplementalAttributeAssociation(1, "Generator", 100, "GeographicInfo")
    )
    rows = JSON.parse(export_time_series_associations_openapi(store))
    @test length(rows) == 1
    row = TimeSeriesModels.decode(TimeSeriesModels.TimeSeriesAssociation, rows[1])
    @test row.value isa TimeSeriesModels.SingleTimeSeries
    rows = JSON.parse(export_supplemental_attribute_associations_openapi(store))
    @test length(rows) == 1
    @test CoreModels.decode(CoreModels.SupplementalAttributeAssociation, rows[1]) isa
        CoreModels.SupplementalAttributeAssociation
    close!(store)
end
